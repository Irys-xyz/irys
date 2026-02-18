pub mod chunks;
pub mod facade;
pub mod ingress_proofs;
pub mod pending_chunks;

pub use chunks::{AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError};
pub use ingress_proofs::{IngressProofError, IngressProofGenerationError};
pub use pending_chunks::PriorityPendingChunks;

use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;

use irys_database::db::IrysDatabaseExt as _;
use irys_domain::{BlockTreeEntry, BlockTreeReadGuard, StorageModulesReadGuard};
use irys_types::ingress::IngressProof;
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, ChunkPathHash, Config, DataRoot,
    TokioServiceHandle, H256,
};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use reth::tasks::TaskExecutor;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{error, info, warn};

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
    async fn handle_message(&self, msg: ChunkIngressMessage) {
        match msg {
            ChunkIngressMessage::IngestChunk(chunk, response) => {
                let result = self.handle_chunk_ingress_message(chunk).await;
                if let Err(ref e) = result {
                    crate::mempool_service::metrics::record_chunk_error(
                        e.error_type(),
                        e.is_advisory(),
                    );
                }
                let _ = response.send(result);
            }
            ChunkIngressMessage::IngestChunkFireAndForget(chunk) => {
                if let Err(e) = self.handle_chunk_ingress_message(chunk).await {
                    crate::mempool_service::metrics::record_chunk_error(
                        e.error_type(),
                        e.is_advisory(),
                    );
                }
            }
            ChunkIngressMessage::IngestIngressProof(proof, response) => {
                let result = self.handle_ingest_ingress_proof(proof);
                let _ = response.send(result);
            }
            ChunkIngressMessage::ProcessPendingChunks(data_root) => {
                self.process_pending_chunks_for_root(data_root).await;
            }
        }
    }

    /// Helper to get the latest block height from the canonical chain.
    pub fn get_latest_block_height_static(
        block_tree_read_guard: &BlockTreeReadGuard,
    ) -> Result<u64, String> {
        let canon_chain = block_tree_read_guard.read().get_canonical_chain();
        let latest = canon_chain
            .0
            .last()
            .ok_or_else(|| "unable to get canonical chain from block tree".to_owned())?;
        Ok(latest.height())
    }

    /// Resolves an anchor (block hash) to its height.
    /// If it couldn't find the anchor, returns None.
    /// Set canonical to true to enforce that the anchor must be part of the current canonical chain.
    pub fn get_anchor_height_static(
        block_tree_read_guard: &BlockTreeReadGuard,
        irys_db: &DatabaseProvider,
        anchor: H256,
        canonical: bool,
    ) -> eyre::Result<Option<u64>> {
        if let Some(height) = {
            let guard = block_tree_read_guard.read();
            if canonical {
                guard
                    .get_canonical_chain()
                    .0
                    .iter()
                    .find(|b| b.block_hash() == anchor)
                    .map(BlockTreeEntry::height)
            } else {
                guard.get_block(&anchor).map(|h| h.height)
            }
        } {
            Ok(Some(height))
        } else if let Some(hdr) =
            irys_db.view_eyre(|tx| irys_database::block_header_by_hash(tx, &anchor, false))?
        {
            Ok(Some(hdr.height))
        } else {
            Ok(None)
        }
    }

    async fn process_pending_chunks_for_root(&self, data_root: DataRoot) {
        let option_chunks_map = self.pending_chunks.write().await.pop(&data_root);
        if let Some(chunks_map) = option_chunks_map {
            let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
            for chunk in chunks {
                if let Err(err) = self.handle_chunk_ingress_message(chunk).await {
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
                        Some(msg) => inner.handle_message(msg).await,
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
