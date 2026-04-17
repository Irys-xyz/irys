pub(crate) mod chunk_data_writer;
pub mod chunks;
pub mod facade;
pub mod ingress_proofs;
pub(crate) mod metrics;
pub mod pending_chunks;

pub use chunks::{AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError};
pub use ingress_proofs::{IngressProofError, IngressProofGenerationError};
pub use pending_chunks::PriorityPendingChunks;

use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use irys_domain::{BlockTreeReadGuard, StorageModulesReadGuard};
use irys_types::ingress::IngressProof;
use irys_types::{
    ChunkPathHash, Config, DataRoot, TokioServiceHandle, Traced, app_state::DatabaseProvider,
    chunk::UnpackedChunk,
};
use lru::LruCache;
use reth::tasks::TaskExecutor;
use reth::tasks::shutdown::Shutdown;
use tokio::sync::{RwLock, Semaphore, mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, error, info, warn};

use crate::mempool_service::wait_with_progress;
use crate::services::ServiceSenders;

/// Messages handled by the ChunkIngressService
#[derive(Debug)]
pub enum ChunkIngressMessage {
    /// Ingest a chunk, optionally with a response channel for the result
    IngestChunk(
        UnpackedChunk,
        Option<oneshot::Sender<Result<(), ChunkIngressError>>>,
    ),
    /// Ingest an ingress proof received from a peer
    IngestIngressProof(IngressProof, oneshot::Sender<Result<(), IngressProofError>>),
    /// Process pending chunks for a data root after its TX header was ingested.
    /// Sent by the mempool when a data TX is successfully validated.
    ProcessPendingChunks(DataRoot),
    /// Try to generate ingress proofs for data roots just confirmed in a block's
    /// submit ledger. Sent by the mempool service after block confirmation.
    TryGenerateProofsForConfirmedRoots(Vec<DataRoot>),
}

impl ChunkIngressMessage {
    /// Returns the variant name as a static string for tracing/logging purposes
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::IngestChunk(_, _) => "IngestChunk",
            Self::IngestIngressProof(_, _) => "IngestIngressProof",
            Self::ProcessPendingChunks(_) => "ProcessPendingChunks",
            Self::TryGenerateProofsForConfirmedRoots(_) => "TryGenerateProofsForConfirmedRoots",
        }
    }

    /// Returns true when the message carries a oneshot reply channel the caller
    /// is awaiting. Fire-and-forget variants (internal work with no caller)
    /// must not be silently dropped under backpressure — there is nobody to
    /// retry them.
    pub fn has_reply_channel(&self) -> bool {
        match self {
            Self::IngestChunk(_, reply) => reply.is_some(),
            Self::IngestIngressProof(_, _) => true,
            Self::ProcessPendingChunks(_) | Self::TryGenerateProofsForConfirmedRoots(_) => false,
        }
    }
}

/// Shared read handle for chunk ingress state.
/// Allows querying pending chunks count without going through the message loop.
#[derive(Debug, Clone)]
pub struct ChunkIngressState {
    pending_chunks: Arc<RwLock<PriorityPendingChunks>>,
}

impl ChunkIngressState {
    pub async fn pending_chunks_count(&self) -> usize {
        self.pending_chunks.read().await.len()
    }
}

pub(crate) struct ChunkIngressServiceInner {
    pub(crate) block_tree_read_guard: BlockTreeReadGuard,
    pub(crate) config: Config,
    pub(crate) exec: TaskExecutor,
    pub(crate) irys_db: DatabaseProvider,
    pub(crate) message_handler_semaphore: Arc<Semaphore>,
    /// Reserved lane for control-plane messages (`IngestIngressProof`,
    /// `ProcessPendingChunks`). Chunk floods saturate
    /// `message_handler_semaphore` but cannot starve the control plane.
    pub(crate) control_plane_semaphore: Arc<Semaphore>,
    pub(crate) max_concurrent_tasks: u32,
    pub(crate) max_control_plane_tasks: u32,
    pub(crate) service_senders: ServiceSenders,
    pub(crate) storage_modules_guard: StorageModulesReadGuard,
    pub(crate) recent_valid_chunks: tokio::sync::RwLock<LruCache<ChunkPathHash, ()>>,
    pub(crate) pending_chunks: Arc<RwLock<PriorityPendingChunks>>,
    pub(crate) chunk_data_writer: chunk_data_writer::ChunkDataWriter,
}

impl ChunkIngressServiceInner {
    /// Pick the semaphore that gates the given message variant. Chunk ingress
    /// uses the main semaphore; control-plane messages use the reserved lane.
    fn semaphore_for(&self, msg: &ChunkIngressMessage) -> Arc<Semaphore> {
        match msg {
            ChunkIngressMessage::IngestChunk(..) => self.message_handler_semaphore.clone(),
            ChunkIngressMessage::IngestIngressProof(..)
            | ChunkIngressMessage::ProcessPendingChunks(..)
            | ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(..) => {
                self.control_plane_semaphore.clone()
            }
        }
    }
}

pub struct ChunkIngressService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<ChunkIngressMessage>>,
    inner: Arc<ChunkIngressServiceInner>,
}

impl ChunkIngressServiceInner {
    async fn handle_message(&self, msg: ChunkIngressMessage) {
        match msg {
            ChunkIngressMessage::IngestChunk(chunk, response) => {
                let result = self.handle_chunk_ingress_message(chunk).await;
                if let Err(e) = &result {
                    metrics::record_chunk_error(e.error_type(), e.is_advisory());
                    if response.is_none() {
                        error!("handle_chunk_ingress_message error: {:?}", e);
                    }
                }
                if let Some(response) = response
                    && response.send(result).is_err()
                {
                    warn!("IngestChunk response channel closed (caller dropped)");
                }
            }
            ChunkIngressMessage::IngestIngressProof(proof, response) => {
                let result = self.handle_ingest_ingress_proof(proof);
                if response.send(result).is_err() {
                    warn!("IngestIngressProof response channel closed (caller dropped)");
                }
            }
            ChunkIngressMessage::ProcessPendingChunks(data_root) => {
                self.process_pending_chunks_for_root(data_root).await;
            }
            ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(data_roots) => {
                // Flush to ensure any buffered chunk writes are visible before reading.
                if let Err(e) = self.chunk_data_writer.flush().await {
                    error!(
                        "Failed to flush chunk data writer before post-confirmation proof check: {:?}",
                        e
                    );
                }
                let chunk_size = self.config.consensus.chunk_size;
                for data_root in data_roots {
                    if let Err(e) = self.try_generate_ingress_proof_for_root(data_root, chunk_size)
                    {
                        warn!(
                            ?data_root,
                            "Failed to generate ingress proof after block confirmation: {:?}", e
                        );
                    }
                }
            }
        }
    }

    async fn process_pending_chunks_for_root(&self, data_root: DataRoot) {
        let option_chunks_map = self.pending_chunks.write().await.pop(&data_root);
        if let Some(chunks_map) = option_chunks_map {
            let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
            for chunk in chunks {
                if let Err(err) = self.handle_chunk_ingress_message(chunk).await {
                    metrics::record_chunk_error(err.error_type(), err.is_advisory());
                    error!(
                        "Failed to handle pending chunk ingress for data_root {:?}: {:?}",
                        data_root, err
                    );
                }
            }
        }
    }
}

impl ChunkIngressService {
    /// Spawn a new ChunkIngressService
    pub fn spawn_service(
        irys_db: DatabaseProvider,
        storage_modules_guard: StorageModulesReadGuard,
        block_tree_read_guard: &BlockTreeReadGuard,
        rx: UnboundedReceiver<Traced<ChunkIngressMessage>>,
        config: &Config,
        service_senders: &ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
        task_executor: TaskExecutor,
    ) -> (TokioServiceHandle, ChunkIngressState) {
        info!("Spawning chunk ingress service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.mempool;
        let max_valid_chunks = mempool_config.max_valid_chunks;
        let max_pending_chunk_items = mempool_config.max_pending_chunk_items;
        let max_preheader_chunks_per_item = mempool_config.max_preheader_chunks_per_item;
        // The only runtime clamp kept here is the u32 ceiling: the semaphore
        // permit count is stored as `u32` for `acquire_many_owned`, so values
        // above `u32::MAX` must be capped. `Config::validate()` already
        // enforces the real invariants (>0 and strict inequality with the
        // control-plane lane), so there is no lower clamp — operator intent
        // is honoured instead of silently overridden.
        const MAX_PERMITS: usize = u32::MAX as usize;
        let raw_max_concurrent = mempool_config.max_concurrent_chunk_ingress_tasks;
        let max_concurrent_chunk_ingress_tasks = raw_max_concurrent.min(MAX_PERMITS);
        if max_concurrent_chunk_ingress_tasks != raw_max_concurrent {
            warn!(
                configured = raw_max_concurrent,
                effective = max_concurrent_chunk_ingress_tasks,
                "Capped max_concurrent_chunk_ingress_tasks at u32::MAX (semaphore permit count ceiling)"
            );
        }
        // Carve the control-plane slice out of the total chunk ingress
        // budget rather than stacking on top. Peak concurrency therefore
        // stays at `max_concurrent_chunk_ingress_tasks`, matching what an
        // operator who tuned that knob to a safe limit would expect. Must
        // leave at least one permit for the chunk lane; clamped to at least
        // 1 for the control plane (mempool config validation also rejects 0).
        let raw_control_plane_tasks = mempool_config.max_control_plane_concurrent_tasks;
        let control_plane_tasks = raw_control_plane_tasks
            .min(max_concurrent_chunk_ingress_tasks.saturating_sub(1))
            .max(1);
        let chunk_lane_tasks = max_concurrent_chunk_ingress_tasks
            .saturating_sub(control_plane_tasks)
            .max(1);
        if control_plane_tasks != raw_control_plane_tasks {
            warn!(
                configured = raw_control_plane_tasks,
                effective = control_plane_tasks,
                total = max_concurrent_chunk_ingress_tasks,
                "Adjusted max_control_plane_concurrent_tasks: carved out of max_concurrent_chunk_ingress_tasks"
            );
        }
        let chunk_writer_buffer_size = mempool_config.chunk_writer_buffer_size;
        let service_senders = service_senders.clone();

        let pending_chunks = Arc::new(RwLock::new(PriorityPendingChunks::new(
            max_pending_chunk_items,
            max_preheader_chunks_per_item,
        )));
        let chunk_ingress_state = ChunkIngressState {
            pending_chunks: pending_chunks.clone(),
        };

        let handle_for_inner = runtime_handle.clone();
        let handle = runtime_handle.spawn(
            async move {
                let recent_valid_chunks = tokio::sync::RwLock::new(LruCache::new(
                    NonZeroUsize::new(max_valid_chunks).unwrap(),
                ));
                let chunk_data_writer = chunk_data_writer::ChunkDataWriter::spawn(
                    irys_db.clone(),
                    chunk_writer_buffer_size,
                    &handle_for_inner,
                );

                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: Arc::new(ChunkIngressServiceInner {
                        block_tree_read_guard,
                        config,
                        exec: task_executor,
                        irys_db,
                        message_handler_semaphore: Arc::new(Semaphore::new(chunk_lane_tasks)),
                        control_plane_semaphore: Arc::new(Semaphore::new(control_plane_tasks)),
                        max_concurrent_tasks: u32::try_from(chunk_lane_tasks)
                            .expect("clamped to u32::MAX above"),
                        max_control_plane_tasks: u32::try_from(control_plane_tasks)
                            .expect("clamped to u32::MAX above"),
                        service_senders,
                        storage_modules_guard,
                        recent_valid_chunks,
                        pending_chunks,
                        chunk_data_writer,
                    }),
                };
                service
                    .start(handle_for_inner)
                    .await
                    .expect("ChunkIngressService encountered an irrecoverable error")
            }
            .instrument(tracing::info_span!("chunk_ingress_service")),
        );

        let handle = TokioServiceHandle {
            name: "chunk_ingress_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        };

        (handle, chunk_ingress_state)
    }

    async fn start(mut self, runtime_handle: tokio::runtime::Handle) -> eyre::Result<()> {
        info!("starting ChunkIngressService");

        let mut shutdown_future = pin!(self.shutdown);
        'service: loop {
            tokio::select! {
                _ = &mut shutdown_future => {
                    info!("ChunkIngressService received shutdown signal");
                    break 'service;
                }
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            let msg_type = msg.variant_name();
                            let has_reply = msg.has_reply_channel();
                            let span = tracing::info_span!(parent: &parent_span, "chunk_ingress_handle_message", msg_type = %msg_type);

                            // Pick the right lane: chunk ingress goes through
                            // the main semaphore; control-plane messages get
                            // the reserved lane that chunk floods cannot starve.
                            let semaphore = self.inner.semaphore_for(&msg);
                            let permit = match Arc::clone(&semaphore).try_acquire_owned() {
                                Ok(permit) => permit,
                                Err(tokio::sync::TryAcquireError::Closed) => {
                                    error!("Chunk ingress message handler semaphore closed");
                                    break 'service;
                                }
                                Err(tokio::sync::TryAcquireError::NoPermits) => {
                                    if has_reply {
                                        // Caller is awaiting a reply — hand back
                                        // a fast advisory so they can retry
                                        // upstream rather than waiting on us.
                                        warn!(
                                            msg_type = %msg_type,
                                            "Chunk ingress service lane saturated, returning Overloaded"
                                        );
                                        Self::send_overloaded_errors(msg);
                                        continue 'service;
                                    }
                                    // No reply channel means internal work
                                    // with no caller to retry. Wait for a
                                    // permit rather than silently drop it.
                                    warn!(
                                        msg_type = %msg_type,
                                        "Chunk ingress service lane saturated, awaiting permit for fire-and-forget message"
                                    );
                                    tokio::select! {
                                        _ = &mut shutdown_future => {
                                            info!("ChunkIngressService received shutdown signal while awaiting permit");
                                            break 'service;
                                        }
                                        res = semaphore.acquire_owned() => match res {
                                            Ok(permit) => permit,
                                            Err(_) => {
                                                error!("Chunk ingress message handler semaphore closed while awaiting permit");
                                                break 'service;
                                            }
                                        }
                                    }
                                }
                            };
                            let inner = Arc::clone(&self.inner);
                            runtime_handle.spawn(async move {
                                let _permit = permit;
                                let task_info = format!("Chunk ingress message handler for {}", msg_type);
                                wait_with_progress(
                                    inner.handle_message(msg),
                                    20,
                                    &task_info,
                                ).await;
                            }.instrument(span));
                        }
                        None => {
                            warn!("ChunkIngressService receiver channel closed");
                            break;
                        }
                    }
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");

        // Phase 1: drain queued messages, spawning concurrently when permits are available
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, parent_span) = traced.into_parts();
            let msg_type = msg.variant_name();
            let span = tracing::info_span!(parent: &parent_span, "chunk_ingress_handle_message", msg_type = %msg_type);

            let inner = Arc::clone(&self.inner);
            let semaphore = inner.semaphore_for(&msg);
            match semaphore.try_acquire_owned() {
                Ok(permit) => {
                    runtime_handle.spawn(
                        async move {
                            let _permit = permit;
                            let task_info = format!("shutdown drain: {}", msg_type);
                            wait_with_progress(inner.handle_message(msg), 20, &task_info).await;
                        }
                        .instrument(span),
                    );
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    error!("Semaphore closed during chunk ingress shutdown drain");
                    Self::send_timeout_errors(msg);
                    break;
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    let task_info = format!("shutdown drain (inline): {}", msg_type);
                    wait_with_progress(inner.handle_message(msg), 20, &task_info)
                        .instrument(span)
                        .await;
                }
            }
        }

        // Phase 2: acquire all permits from both lanes to wait for in-flight
        // and drain-spawned handlers. Use `acquire_many_owned` so the permits
        // are not lifetime-tied to the semaphore arcs.
        let chunk_acquire = self
            .inner
            .message_handler_semaphore
            .clone()
            .acquire_many_owned(self.inner.max_concurrent_tasks);
        let control_acquire = self
            .inner
            .control_plane_semaphore
            .clone()
            .acquire_many_owned(self.inner.max_control_plane_tasks);
        let drain_fut = async move {
            let chunk_permits = chunk_acquire.await?;
            let control_permits = control_acquire.await?;
            Ok::<_, tokio::sync::AcquireError>((chunk_permits, control_permits))
        };
        let handlers_quiesced = match tokio::time::timeout(Duration::from_secs(30), drain_fut).await
        {
            Ok(Ok(all_permits)) => {
                tracing::debug!("All chunk ingress handlers completed");
                let _all_permits = all_permits;
                true
            }
            Ok(Err(_)) => {
                error!("Semaphore closed during chunk ingress shutdown drain");
                false
            }
            Err(_) => {
                warn!("Timed out waiting for in-flight chunk ingress handlers; skipping flush");
                false
            }
        };

        if handlers_quiesced && let Err(e) = self.inner.chunk_data_writer.flush().await {
            warn!("Failed to flush chunk writer on shutdown: {:?}", e);
        }

        info!("ChunkIngressService shut down");
        Ok(())
    }

    /// Send explicit timeout errors through any oneshot channels in a message
    /// before dropping it, so callers get a descriptive error instead of a
    /// generic `RecvError` from a silently dropped sender.
    fn send_timeout_errors(msg: ChunkIngressMessage) {
        match msg {
            ChunkIngressMessage::IngestChunk(_, Some(reply)) => {
                let _ = reply.send(Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::Other(
                        "service overloaded: timed out waiting for handler permit".into(),
                    ),
                )));
            }
            ChunkIngressMessage::IngestIngressProof(_, reply) => {
                let _ = reply.send(Err(IngressProofError::Other(
                    "service overloaded: timed out waiting for handler permit".into(),
                )));
            }
            // No response channel — nothing to notify.
            ChunkIngressMessage::IngestChunk(_, None)
            | ChunkIngressMessage::ProcessPendingChunks(_)
            | ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(_) => {}
        }
    }

    /// Send `Overloaded` advisory errors through any oneshot channels in a
    /// message so the caller gets a fast, retryable response instead of waiting
    /// for the service to clear. The advisory variant is score-neutral upstream,
    /// so peers are not penalised for hitting our backpressure.
    fn send_overloaded_errors(msg: ChunkIngressMessage) {
        match msg {
            ChunkIngressMessage::IngestChunk(_, Some(reply)) => {
                let _ = reply.send(Err(ChunkIngressError::Advisory(
                    AdvisoryChunkIngressError::Overloaded,
                )));
            }
            ChunkIngressMessage::IngestIngressProof(_, reply) => {
                let _ = reply.send(Err(IngressProofError::Overloaded));
            }
            // No response channel — nothing to notify.
            ChunkIngressMessage::IngestChunk(_, None)
            | ChunkIngressMessage::ProcessPendingChunks(_)
            | ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(_) => {}
        }
    }
}

#[cfg(test)]
mod overload_helpers_tests {
    use super::*;
    use irys_types::{DataRoot, H256, IrysSignature, ingress::IngressProof};

    fn dummy_chunk() -> UnpackedChunk {
        UnpackedChunk {
            data_root: DataRoot::from([0_u8; 32]),
            data_size: 0,
            tx_offset: 0_u32.into(),
            data_path: Default::default(),
            bytes: Default::default(),
        }
    }

    fn dummy_ingress_proof() -> IngressProof {
        IngressProof::V1(irys_types::ingress::IngressProofV1 {
            signature: IrysSignature::default(),
            data_root: H256::zero(),
            proof: H256::zero(),
            chain_id: 0,
            anchor: H256::zero(),
        })
    }

    /// `send_overloaded_errors` must return the advisory `Overloaded` variant
    /// to a chunk caller that supplied a reply channel. The advisory variant
    /// is score-neutral upstream, so peers will retry rather than penalise.
    #[tokio::test]
    async fn ingest_chunk_overloaded_returns_advisory_overloaded() {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ChunkIngressMessage::IngestChunk(dummy_chunk(), Some(reply_tx));

        ChunkIngressService::send_overloaded_errors(msg);

        let result = reply_rx.await.expect("oneshot must be resolved");
        match result {
            Err(ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded)) => {}
            other => panic!("expected Advisory(Overloaded), got {:?}", other),
        }
    }

    /// `send_overloaded_errors` must not panic on chunk messages with no reply
    /// channel — there is simply no caller to notify.
    #[tokio::test]
    async fn ingest_chunk_overloaded_no_reply_is_noop() {
        let msg = ChunkIngressMessage::IngestChunk(dummy_chunk(), None);
        ChunkIngressService::send_overloaded_errors(msg);
    }

    /// Control-plane `IngestIngressProof` callers must also receive a fast
    /// failure (not a silent drop) when the control lane is saturated. The
    /// dedicated `Overloaded` variant lets callers distinguish backpressure
    /// from generic errors without string matching.
    #[tokio::test]
    async fn ingest_ingress_proof_overloaded_returns_overloaded() {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = ChunkIngressMessage::IngestIngressProof(dummy_ingress_proof(), reply_tx);

        ChunkIngressService::send_overloaded_errors(msg);

        let result = reply_rx.await.expect("oneshot must be resolved");
        assert!(matches!(result, Err(IngressProofError::Overloaded)));
    }

    /// `ProcessPendingChunks` has no response channel, so `send_overloaded_errors`
    /// is a no-op for it — the pending chunks stay queued and will be
    /// processed on a future trigger. Must not panic.
    #[tokio::test]
    async fn process_pending_chunks_overloaded_is_noop() {
        let msg = ChunkIngressMessage::ProcessPendingChunks(DataRoot::from([1_u8; 32]));
        ChunkIngressService::send_overloaded_errors(msg);
    }

    /// `Overloaded` exposes its own `error_type()` string so dashboards can
    /// distinguish it from other advisory errors.
    #[test]
    fn overloaded_error_type_is_distinct() {
        assert_eq!(
            AdvisoryChunkIngressError::Overloaded.error_type(),
            "overloaded"
        );
    }

    /// `has_reply_channel` must classify variants correctly so the service
    /// loop can decide between fast-fail (caller-retryable) and block-on-permit
    /// (fire-and-forget) under saturation. Reply-less variants must return
    /// `false` — dropping them silently under `NoPermits` would lose internal
    /// work (`ProcessPendingChunks` from the mempool, sync-path fallback
    /// `IngestChunk(_, None)`) with nothing to trigger a retry.
    #[test]
    fn has_reply_channel_distinguishes_fire_and_forget_from_reply_bearing() {
        let (reply_tx, _reply_rx) = oneshot::channel();
        assert!(
            ChunkIngressMessage::IngestChunk(dummy_chunk(), Some(reply_tx)).has_reply_channel()
        );
        assert!(!ChunkIngressMessage::IngestChunk(dummy_chunk(), None).has_reply_channel());

        let (reply_tx, _reply_rx) = oneshot::channel();
        assert!(
            ChunkIngressMessage::IngestIngressProof(dummy_ingress_proof(), reply_tx)
                .has_reply_channel()
        );

        assert!(
            !ChunkIngressMessage::ProcessPendingChunks(DataRoot::from([1_u8; 32]))
                .has_reply_channel()
        );
    }
}
