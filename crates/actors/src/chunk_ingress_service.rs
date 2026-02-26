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
    app_state::DatabaseProvider, chunk::UnpackedChunk, ChunkPathHash, Config, DataRoot,
    TokioServiceHandle, Traced,
};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use reth::tasks::TaskExecutor;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot, RwLock, Semaphore};
use tracing::{error, info, warn, Instrument as _};

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
}

impl ChunkIngressMessage {
    /// Returns the variant name as a static string for tracing/logging purposes
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::IngestChunk(_, _) => "IngestChunk",
            Self::IngestIngressProof(_, _) => "IngestIngressProof",
            Self::ProcessPendingChunks(_) => "ProcessPendingChunks",
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
    pub(crate) max_concurrent_tasks: u32,
    pub(crate) service_senders: ServiceSenders,
    pub(crate) storage_modules_guard: StorageModulesReadGuard,
    pub(crate) recent_valid_chunks: tokio::sync::RwLock<LruCache<ChunkPathHash, ()>>,
    pub(crate) pending_chunks: Arc<RwLock<PriorityPendingChunks>>,
    pub(crate) chunk_data_writer: chunk_data_writer::ChunkDataWriter,
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
                if let Err(ref e) = &result {
                    metrics::record_chunk_error(e.error_type(), e.is_advisory());
                    if response.is_none() {
                        error!("handle_chunk_ingress_message error: {:?}", e);
                    }
                }
                if let Some(response) = response {
                    if response.send(result).is_err() {
                        warn!("IngestChunk response channel closed (caller dropped)");
                    }
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
    ) -> (TokioServiceHandle, ChunkIngressState) {
        info!("Spawning chunk ingress service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.mempool;
        let max_valid_chunks = mempool_config.max_valid_chunks;
        let max_pending_chunk_items = mempool_config.max_pending_chunk_items;
        let max_preheader_chunks_per_item = mempool_config.max_preheader_chunks_per_item;
        let raw_max_concurrent = mempool_config.max_concurrent_chunk_ingress_tasks;
        const MAX_PERMITS: usize = u32::MAX as usize;
        const MIN_CONCURRENT: usize = 20;
        let max_concurrent_chunk_ingress_tasks =
            raw_max_concurrent.clamp(MIN_CONCURRENT, MAX_PERMITS);
        if max_concurrent_chunk_ingress_tasks != raw_max_concurrent {
            warn!(
                configured = raw_max_concurrent,
                effective = max_concurrent_chunk_ingress_tasks,
                "Adjusted max_concurrent_chunk_ingress_tasks to supported range {MIN_CONCURRENT}..=u32::MAX"
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
                );

                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: Arc::new(ChunkIngressServiceInner {
                        block_tree_read_guard,
                        config,
                        exec: TaskExecutor::current(),
                        irys_db,
                        message_handler_semaphore: Arc::new(Semaphore::new(
                            max_concurrent_chunk_ingress_tasks,
                        )),
                        max_concurrent_tasks: u32::try_from(max_concurrent_chunk_ingress_tasks)
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
        loop {
            tokio::select! {
                _ = &mut shutdown_future => {
                    info!("ChunkIngressService received shutdown signal");
                    break;
                }
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            let msg_type = msg.variant_name();
                            let span = tracing::info_span!(parent: &parent_span, "chunk_ingress_handle_message", msg_type = %msg_type);

                            let semaphore = self.inner.message_handler_semaphore.clone();
                            match semaphore.try_acquire_owned() {
                                Ok(permit) => {
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
                                Err(e) => {
                                    match e {
                                        tokio::sync::TryAcquireError::Closed => {
                                            error!("Chunk ingress message handler semaphore closed");
                                            break;
                                        }
                                        tokio::sync::TryAcquireError::NoPermits => {
                                            warn!("Chunk ingress message handler semaphore at capacity, waiting for permit");
                                        }
                                    }
                                    // Await inline (blocking the loop) for natural backpressure,
                                    // matching the mempool service pattern.
                                    let inner = Arc::clone(&self.inner);
                                    let semaphore = inner.message_handler_semaphore.clone();
                                    match tokio::time::timeout(Duration::from_secs(60), semaphore.acquire_owned()).await {
                                        Ok(Ok(permit)) => {
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
                                        Ok(Err(err)) => {
                                            error!("Failed to acquire chunk ingress message handler permit: {:?}", err);
                                            Self::send_timeout_errors(msg);
                                        }
                                        Err(_) => {
                                            warn!("Timed out waiting for chunk ingress message handler permit, dropping message");
                                            Self::send_timeout_errors(msg);
                                        }
                                    }
                                }
                            }
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

        // Phase 1: wait for in-flight spawned handlers to finish
        let acquire_fut = self
            .inner
            .message_handler_semaphore
            .acquire_many(self.inner.max_concurrent_tasks);
        let _all_permits = match tokio::time::timeout(Duration::from_secs(30), acquire_fut).await {
            Ok(Ok(p)) => Some(p),
            Ok(Err(_)) => {
                error!("Semaphore closed during chunk ingress shutdown drain");
                None
            }
            Err(_) => {
                warn!("Timed out waiting for in-flight chunk ingress handlers; proceeding without full drain");
                None
            }
        };

        // Phase 2: drain queued messages with its own budget
        let drain_fut = async {
            while let Ok(traced) = self.msg_rx.try_recv() {
                let (msg, parent_span) = traced.into_parts();
                let msg_type = msg.variant_name();
                let span = tracing::info_span!(parent: &parent_span, "chunk_ingress_handle_message", msg_type = %msg_type);
                let task_info = format!("shutdown drain: {}", msg_type);
                wait_with_progress(self.inner.handle_message(msg), 20, &task_info)
                    .instrument(span)
                    .await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(10), drain_fut).await {
            Ok(()) => tracing::debug!("Processed remaining chunk ingress messages successfully"),
            Err(_) => {
                warn!("Timeout draining remaining chunk ingress messages, continuing shutdown")
            }
        }

        if let Err(e) = self.inner.chunk_data_writer.flush().await {
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
            | ChunkIngressMessage::ProcessPendingChunks(_) => {}
        }
    }
}
