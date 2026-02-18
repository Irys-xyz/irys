use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;

use irys_domain::{BlockTreeReadGuard, StorageModulesReadGuard};
use irys_types::ingress::IngressProof;
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, ChunkPathHash, Config, DataRoot,
    TokioServiceHandle,
};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use reth::tasks::TaskExecutor;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{info, warn};

use crate::mempool_service::{
    ChunkIngressError, CriticalChunkIngressError, IngressProofError, PriorityPendingChunks,
};
use crate::services::ServiceSenders;

/// Messages handled by the ChunkIngressService
#[derive(Debug)]
pub enum ChunkIngressMessage {
    /// Ingest a chunk with a response channel for the result
    IngestChunk(
        UnpackedChunk,
        oneshot::Sender<Result<(), ChunkIngressError>>,
    ),
    /// Ingest a chunk without waiting for a response (fire-and-forget)
    IngestChunkFireAndForget(UnpackedChunk),
    /// Ingest an ingress proof received from a peer
    IngestIngressProof(IngressProof, oneshot::Sender<Result<(), IngressProofError>>),
    /// Process pending chunks for a data root after its TX header was ingested.
    /// Sent by the mempool when a data TX is successfully validated.
    ProcessPendingChunks(DataRoot),
}

pub struct ChunkIngressServiceInner {
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub config: Config,
    pub exec: TaskExecutor,
    pub irys_db: DatabaseProvider,
    pub service_senders: ServiceSenders,
    pub storage_modules_guard: StorageModulesReadGuard,
    pub recent_valid_chunks: tokio::sync::RwLock<LruCache<ChunkPathHash, ()>>,
    pub pending_chunks: tokio::sync::RwLock<PriorityPendingChunks>,
}

pub struct ChunkIngressService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<ChunkIngressMessage>,
    inner: Arc<ChunkIngressServiceInner>,
}

impl ChunkIngressServiceInner {
    fn handle_message(&self, msg: ChunkIngressMessage) {
        match msg {
            ChunkIngressMessage::IngestChunk(_, response) => {
                warn!("ChunkIngressService: IngestChunk not yet implemented");
                let _ = response.send(Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::ServiceUninitialized,
                )));
            }
            ChunkIngressMessage::IngestChunkFireAndForget(_) => {
                warn!("ChunkIngressService: IngestChunkFireAndForget not yet implemented");
            }
            ChunkIngressMessage::IngestIngressProof(_, response) => {
                warn!("ChunkIngressService: IngestIngressProof not yet implemented");
                let _ = response.send(Err(IngressProofError::Other(
                    "ChunkIngressService not yet implemented".to_string(),
                )));
            }
            ChunkIngressMessage::ProcessPendingChunks(_) => {
                warn!("ChunkIngressService: ProcessPendingChunks not yet implemented");
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
        rx: UnboundedReceiver<ChunkIngressMessage>,
        config: &Config,
        service_senders: &ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning chunk ingress service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.mempool;
        let max_valid_chunks = mempool_config.max_valid_chunks;
        let max_pending_chunk_items = mempool_config.max_pending_chunk_items;
        let max_preheader_chunks_per_item = mempool_config.max_preheader_chunks_per_item;
        let service_senders = service_senders.clone();

        let handle = runtime_handle.spawn(async move {
            let recent_valid_chunks = tokio::sync::RwLock::new(LruCache::new(
                NonZeroUsize::new(max_valid_chunks).unwrap(),
            ));
            let pending_chunks = tokio::sync::RwLock::new(PriorityPendingChunks::new(
                max_pending_chunk_items,
                max_preheader_chunks_per_item,
            ));

            let service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                inner: Arc::new(ChunkIngressServiceInner {
                    block_tree_read_guard,
                    config,
                    exec: TaskExecutor::current(),
                    irys_db,
                    service_senders,
                    storage_modules_guard,
                    recent_valid_chunks,
                    pending_chunks,
                }),
            };
            service
                .start()
                .await
                .expect("ChunkIngressService encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "chunk_ingress_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(self) -> eyre::Result<()> {
        info!("starting ChunkIngressService");

        let Self {
            shutdown,
            mut msg_rx,
            inner,
        } = self;

        let mut shutdown_future = pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown_future => {
                    info!("ChunkIngressService received shutdown signal");
                    break;
                }
                msg = msg_rx.recv() => {
                    match msg {
                        Some(msg) => inner.handle_message(msg),
                        None => {
                            warn!("ChunkIngressService receiver channel closed");
                            break;
                        }
                    }
                }
            }
        }

        info!("ChunkIngressService shut down");
        Ok(())
    }
}
