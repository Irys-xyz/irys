use eyre::eyre;
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockIndex};
use irys_types::{
    BlockHash, BlockIndexItem, ConsensusConfig, DataTransactionHeader, IrysBlockHeader,
    TokioServiceHandle, H256, U256,
};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{error, info, instrument, warn, Instrument as _};

/// Messages supported by the BlockIndex Tokio service
#[derive(Debug)]
pub enum BlockIndexServiceMessage {
    /// Retrieve a read-only guard for the in-memory block index
    GetBlockIndexReadGuard {
        response: oneshot::Sender<BlockIndexReadGuard>,
    },

    /// Migrate a block into the block index (pushes a new BlockIndexItem)
    /// Uses the same payload as the legacy Actix message
    MigrateBlock {
        block_header: Arc<IrysBlockHeader>,
        all_txs: Arc<Vec<DataTransactionHeader>>,
        response: oneshot::Sender<eyre::Result<()>>,
    },

    /// Retrieve the latest BlockIndexItem (if any)
    GetLatestBlockIndex {
        response: oneshot::Sender<Option<BlockIndexItem>>,
    },
}

/// Tokio service that owns the message loop
#[derive(Debug)]
pub struct BlockIndexService {
    shutdown: reth::tasks::shutdown::Shutdown,
    msg_rx: UnboundedReceiver<BlockIndexServiceMessage>,
    inner: BlockIndexServiceInner,
}

#[derive(Debug, Default)]
struct BlockLogEntry {
    #[expect(dead_code)]
    pub block_hash: H256,
    #[expect(dead_code)]
    pub height: u64,
    #[expect(dead_code)]
    pub timestamp: u128,
    #[expect(dead_code)]
    pub difficulty: U256,
}

/// Core logic of the BlockIndex service
#[derive(Debug)]
pub struct BlockIndexServiceInner {
    block_index: Arc<RwLock<BlockIndex>>,
    block_log: Vec<BlockLogEntry>,
    num_blocks: u64,
    chunk_size: u64,
    last_received_block: Option<(u64, BlockHash)>,
}

impl BlockIndexServiceInner {
    pub fn new(block_index: Arc<RwLock<BlockIndex>>, consensus_config: &ConsensusConfig) -> Self {
        Self {
            block_index,
            block_log: Vec::new(),
            num_blocks: 0,
            chunk_size: consensus_config.chunk_size,
            last_received_block: None,
        }
    }

    /// Handle an inbound service message
    #[instrument(skip_all, err)]
    pub fn handle_message(&mut self, msg: BlockIndexServiceMessage) -> eyre::Result<()> {
        match msg {
            BlockIndexServiceMessage::GetBlockIndexReadGuard { response } => {
                let guard = BlockIndexReadGuard::new(self.block_index.clone());
                let _ = response.send(guard);
                Ok(())
            }
            BlockIndexServiceMessage::MigrateBlock {
                block_header,
                all_txs,
                response,
            } => {
                // Maintain simple ordering invariant (sequential heights)
                if let Some((prev_height, prev_hash)) = &self.last_received_block {
                    if block_header.height != prev_height + 1 {
                        let err = eyre!(
                            "Block migration out of order or with a gap: prev_height={}, prev_hash={:x}, current_height={}, current_hash={:x}",
                            prev_height,
                            prev_hash,
                            block_header.height,
                            block_header.block_hash
                        );
                        // notify caller, then exit service by returning Err
                        let _ = response.send(Err(eyre!(err.to_string())));
                        return Err(err);
                    }
                } else {
                    info!(
                        "BlockIndexService received its first block: height {}, hash {:x}",
                        block_header.height, block_header.block_hash
                    );
                }

                // Perform the migration; if it fails, notify caller and exit service
                match self.migrate_block(&block_header, &all_txs) {
                    Ok(()) => {
                        let _ = response.send(Ok(()));
                        Ok(())
                    }
                    Err(e) => {
                        // notify caller, then exit service by returning Err
                        let _ = response.send(Err(eyre!(e.to_string())));
                        Err(e)
                    }
                }
            }
            BlockIndexServiceMessage::GetLatestBlockIndex { response } => {
                let bi = self
                    .block_index
                    .read()
                    .map_err(|_| eyre!("block_index read lock poisoned"))?;
                let block_height = bi.num_blocks().max(1) - 1;
                let resp = bi.get_item(block_height).cloned();
                let _ = response.send(resp);
                Ok(())
            }
        }
    }

    /// Adds a migrated block and its associated transactions to the block index.
    ///
    /// Safety
    /// - Expects `all_txs` to contain transaction headers for every transaction ID in the block's
    ///   Submit and Publish ledgers. This is normally guaranteed by prior validation.
    #[instrument(skip_all, err, fields(height = %block.height, hash = %block.block_hash))]
    pub fn migrate_block(
        &mut self,
        block: &Arc<IrysBlockHeader>,
        all_txs: &Arc<Vec<DataTransactionHeader>>,
    ) -> eyre::Result<()> {
        let chunk_size = self.chunk_size;

        self.block_index
            .write()
            .map_err(|_| eyre!("block_index write lock poisoned"))?
            .push_block(block, all_txs, chunk_size)?;

        self.last_received_block = Some((block.height, block.block_hash));

        // Track a small window of recent blocks for debugging
        self.block_log.push(BlockLogEntry {
            block_hash: block.block_hash,
            height: block.height,
            timestamp: block.timestamp,
            difficulty: block.diff,
        });

        if self.block_log.len() > 20 {
            // keep only the last 20 entries
            self.block_log.drain(0..self.block_log.len() - 20);
        }

        self.num_blocks += 1;

        Ok(())
    }
}

impl BlockIndexService {
    /// Spawns the BlockIndex service on the provided Tokio runtime handle
    ///
    /// Returns a handle that can be used to shut down the service.
    pub fn spawn_service(
        rx: UnboundedReceiver<BlockIndexServiceMessage>,
        block_index: Arc<RwLock<BlockIndex>>,
        consensus_config: &ConsensusConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning BlockIndex service");
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let inner = BlockIndexServiceInner::new(block_index, consensus_config);

        let handle = runtime_handle.spawn(
            async move {
                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner,
                };
                service
                    .start()
                    .await
                    .expect("BlockIndex service encountered an irrecoverable error")
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "block_index_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting BlockIndex service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for BlockIndex service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if let Err(e) = self.inner.handle_message(msg) {
                                error!("Error handling BlockIndex message: {:?}", e);
                                break;
                            }
                        }
                        None => {
                            warn!("BlockIndex message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        // Best-effort drain before shutdown
        while let Ok(msg) = self.msg_rx.try_recv() {
            if let Err(e) = self.inner.handle_message(msg) {
                error!(
                    "Error handling BlockIndex message during shutdown drain: {:?}",
                    e
                );
            }
        }

        info!("Shutting down BlockIndex service gracefully");
        Ok(())
    }
}
