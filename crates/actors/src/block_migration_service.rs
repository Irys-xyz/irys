use crate::{
    block_index_service::BlockIndexServiceMessage,
    chunk_migration_service::ChunkMigrationServiceMessage, mempool_service::MempoolServiceMessage,
    services::ServiceSenders,
};
use eyre::{ensure, OptionExt as _};
use irys_database::{db::IrysDatabaseExt as _, insert_commitment_tx, insert_tx_header};
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockTreeReadGuard};
use irys_types::{
    app_state::DatabaseProvider, BlockTransactions, CommitmentTransaction, DataLedger,
    DataTransactionHeader, IrysBlockHeader, TokioServiceHandle, H256,
};
use reth::tasks::shutdown::Shutdown;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{debug, error, info, Instrument as _};

/// Messages handled by the BlockMigrationService
#[derive(Debug)]
pub enum BlockMigrationServiceMessage {
    /// Migrate all blocks up to and including the given migration block
    MigrateToBlock {
        migration_block: Arc<IrysBlockHeader>,
        response: oneshot::Sender<eyre::Result<()>>,
    },
}

/// Actor service that owns block migration orchestration and DB persistence.
///
/// Responsibilities:
/// - Validates migration continuity (height + chain linkage)
/// - Collects blocks to migrate by walking the block tree
/// - Persists all transaction data and block headers to the database
/// - Notifies BlockIndexService (sync) and ChunkMigrationService (fire-and-forget)
/// - Sends cleanup message to MempoolService (fire-and-forget)
pub struct BlockMigrationService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<BlockMigrationServiceMessage>,
    inner: BlockMigrationInner,
}

pub struct BlockMigrationInner {
    db: DatabaseProvider,
    block_tree_guard: BlockTreeReadGuard,
    block_index_guard: BlockIndexReadGuard,
    service_senders: ServiceSenders,
}

impl BlockMigrationService {
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_block_migration")]
    pub fn spawn_service(
        rx: UnboundedReceiver<BlockMigrationServiceMessage>,
        db: DatabaseProvider,
        block_tree_guard: BlockTreeReadGuard,
        block_index_guard: BlockIndexReadGuard,
        service_senders: ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning block migration service");
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let inner = BlockMigrationInner {
            db,
            block_tree_guard,
            block_index_guard,
            service_senders,
        };

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
                    .expect("BlockMigration service encountered an irrecoverable error")
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "block_migration_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting BlockMigration service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for BlockMigration service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            tracing::warn!("BlockMigration message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        // Best-effort drain before shutdown
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        info!("Shutting down BlockMigration service gracefully");
        Ok(())
    }
}

impl BlockMigrationInner {
    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn handle_message(&self, msg: BlockMigrationServiceMessage) -> eyre::Result<()> {
        match msg {
            BlockMigrationServiceMessage::MigrateToBlock {
                migration_block,
                response,
            } => {
                let result = self.handle_migrate_to_block(&migration_block).await;
                if let Err(ref e) = result {
                    error!(
                        "Block migration failed for block {} (height {}): {:?}",
                        migration_block.block_hash, migration_block.height, e
                    );
                }
                if let Err(_result) = response.send(result) {
                    tracing::warn!("Block migration response channel was closed by receiver");
                }
            }
        }
        Ok(())
    }

    /// Main migration handler: validates continuity, collects blocks, persists to DB,
    /// and notifies downstream services.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %migration_block.block_hash, block.height = migration_block.height))]
    async fn handle_migrate_to_block(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        // 1. Validate migration continuity
        self.validate_migration_continuity(migration_block)?;

        // 2. Collect blocks to migrate (oldest first)
        let blocks_to_migrate = self.get_blocks_to_migrate(migration_block)?;

        debug!(
            block.hash = %migration_block.block_hash,
            block.height = migration_block.height,
            "migrating {} block(s)",
            blocks_to_migrate.len()
        );

        // 3. Process each block in order (oldest to newest)
        for block_to_migrate in blocks_to_migrate {
            // 3a. Extract transactions from block tree cache
            let transactions = {
                let cache = self.block_tree_guard.read();
                cache
                    .blocks
                    .get(&block_to_migrate.block_hash)
                    .map(|meta| meta.transactions.clone())
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "missing cache entry for block {} during block migration",
                            block_to_migrate.block_hash
                        )
                    })?
            };

            // 3b. Verify POA chunk is present
            eyre::ensure!(
                block_to_migrate.poa.chunk.is_some(),
                "poa chunk must be present for block {} at height {}",
                block_to_migrate.block_hash,
                block_to_migrate.height
            );

            // 3c. Persist all transactions and block header to DB
            self.persist_block_to_db(&block_to_migrate, &transactions)?;

            // 3d. Notify BlockIndexService (synchronous — wait for completion)
            self.send_block_index_migration(&block_to_migrate, &transactions)
                .await?;

            // 3e. Notify ChunkMigrationService (fire-and-forget)
            self.send_chunk_migration(&block_to_migrate, &transactions)?;

            // 3f. Notify MempoolService for in-memory cleanup (fire-and-forget)
            self.send_mempool_cleanup(&block_to_migrate, &transactions)?;
        }

        Ok(())
    }

    /// Ensures blocks can be migrated by verifying height continuity and chain linkage.
    fn validate_migration_continuity(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        let blocks_to_migrate = self.get_blocks_to_migrate(migration_block)?;

        // Nothing to validate if no blocks need migration
        let Some(first_block) = blocks_to_migrate.first() else {
            return Ok(());
        };

        let block_index = self.block_index_guard.read();
        let latest_indexed = block_index
            .get_latest_item()
            .ok_or_eyre("Block index is empty")?;

        // Ensure continuous height progression
        ensure!(
            block_index.latest_height() + 1 == first_block.height,
            "Height gap detected: block index at height {} ({}), trying to migrate height {} ({})",
            &block_index.latest_height(),
            &latest_indexed.block_hash,
            &first_block.height,
            &first_block.block_hash
        );

        // Ensure proper chain linkage
        ensure!(
            latest_indexed.block_hash == first_block.previous_block_hash,
            "Chain break detected: migration block ({}) doesn't link to indexed chain ({})",
            &latest_indexed.block_hash,
            &first_block.previous_block_hash
        );

        Ok(())
    }

    /// Collect blocks to migrate by walking backwards from the current migration block
    /// to the head of the `block_index`.
    fn get_blocks_to_migrate(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<Vec<Arc<IrysBlockHeader>>> {
        let mut blocks_to_migrate = vec![];

        // Get the last migrated block hash
        let bi = self.block_index_guard.read();
        let last_migrated = bi
            .get_latest_item()
            .ok_or_eyre("must have at least a single item in block index")?;
        let last_migrated_hash = last_migrated.block_hash;
        drop(bi);

        // Get the block tree
        let block_tree = self.block_tree_guard.read();

        // Walk backwards from the given block to the last migrated block
        let mut current_hash = migration_block.block_hash;

        while current_hash != last_migrated_hash {
            let current_block = block_tree.get_block(&current_hash).ok_or_else(|| {
                eyre::eyre!(
                    "block {} not found while collecting blocks for migration",
                    current_hash
                )
            })?;

            let arc_block = Arc::new(current_block.clone());
            blocks_to_migrate.push(arc_block);

            current_hash = current_block.previous_block_hash;
        }

        // Reverse to get oldest-first order
        blocks_to_migrate.reverse();
        Ok(blocks_to_migrate)
    }

    /// Persists commitment txs, data txs (submit + publish), metadata, and block header to DB.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = block.height))]
    fn persist_block_to_db(
        &self,
        block: &Arc<IrysBlockHeader>,
        transactions: &BlockTransactions,
    ) -> eyre::Result<()> {
        // Stage 1: Insert commitment transactions
        let commitment_txs = &transactions.commitment_txs;
        let commitment_tx_ids: Vec<H256> = commitment_txs
            .iter()
            .map(CommitmentTransaction::id)
            .collect();

        self.db.update_eyre(|tx| {
            for commitment_tx in commitment_txs {
                insert_commitment_tx(tx, commitment_tx)?;
            }
            Ok(())
        })?;

        // Stage 2: Insert submit transactions
        let submit_txs = transactions
            .data_txs
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        let submit_tx_ids: Vec<H256> = submit_txs.iter().map(|tx| tx.id).collect();

        self.db.update_eyre(|tx| {
            for header in &submit_txs {
                insert_tx_header(tx, header)?;
            }
            Ok(())
        })?;

        // Stage 3: Insert publish transactions (with promoted_height)
        let publish_txs = transactions
            .data_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();
        let publish_tx_ids: Vec<H256> = publish_txs.iter().map(|tx| tx.id).collect();

        self.db.update_eyre(|mut_tx| {
            for mut header in publish_txs {
                if header.promoted_height().is_none() {
                    header.metadata_mut().promoted_height = Some(block.height);
                }
                insert_tx_header(mut_tx, &header)?;
            }
            Ok(())
        })?;

        // Stage 4: Batch set metadata (included_height for all, promoted_height for publish)
        let block_height = block.height;

        let all_data_tx_ids: Vec<H256> = submit_tx_ids
            .iter()
            .chain(publish_tx_ids.iter())
            .copied()
            .collect();

        if !all_data_tx_ids.is_empty() || !commitment_tx_ids.is_empty() {
            if let Err(e) = self.db.update_eyre(|tx| {
                if !all_data_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_included_height(
                        tx,
                        &all_data_tx_ids,
                        block_height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                if !commitment_tx_ids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        &commitment_tx_ids,
                        block_height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                Ok(())
            }) {
                error!("Failed to batch set included_height in database: {}", e);
            }
        }

        if !publish_tx_ids.is_empty() {
            if let Err(e) = self.db.update_eyre(|tx| {
                irys_database::batch_set_data_tx_promoted_height(tx, &publish_tx_ids, block_height)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                error!("Failed to batch set promoted_height in database: {}", e);
            }
        }

        // Stage 5: Insert block header with optional POA chunk
        let migrated_block = (*block).clone();
        self.db
            .update_eyre(|tx| irys_database::insert_block_header(tx, &migrated_block))?;

        Ok(())
    }

    /// Send migration notification to BlockIndexService (synchronous — waits for response).
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = block.height))]
    async fn send_block_index_migration(
        &self,
        block: &Arc<IrysBlockHeader>,
        transactions: &BlockTransactions,
    ) -> eyre::Result<()> {
        let submit_txs = transactions
            .data_txs
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        let publish_txs = transactions
            .data_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();

        let mut all_txs = vec![];
        all_txs.extend(publish_txs);
        all_txs.extend(submit_txs);

        info!(
            "Migrating to block_index - hash: {} height: {}",
            &block.block_hash, &block.height
        );

        let (tx, rx) = oneshot::channel();
        self.service_senders
            .block_index
            .send(BlockIndexServiceMessage::MigrateBlock {
                block_header: Arc::clone(block),
                all_txs: Arc::new(all_txs),
                response: tx,
            })?;
        rx.await
            .map_err(|e| eyre::eyre!("Failed to receive BlockIndexService response: {e}"))?
            .map_err(|e| eyre::eyre!("BlockIndexService error during migration: {e}"))?;

        Ok(())
    }

    /// Notify ChunkMigrationService about the migrated block (fire-and-forget).
    fn send_chunk_migration(
        &self,
        block: &Arc<IrysBlockHeader>,
        transactions: &BlockTransactions,
    ) -> eyre::Result<()> {
        let submit_txs = transactions
            .data_txs
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        let publish_txs = transactions
            .data_txs
            .get(&DataLedger::Publish)
            .cloned()
            .unwrap_or_default();

        let mut all_txs_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        all_txs_map.insert(DataLedger::Submit, submit_txs);
        all_txs_map.insert(DataLedger::Publish, publish_txs);

        self.service_senders
            .chunk_migration
            .send(ChunkMigrationServiceMessage::BlockMigrated(
                Arc::clone(block),
                Arc::new(all_txs_map),
            ))
            .map_err(|e| eyre::eyre!("Failed to send BlockMigrated message: {}", e))?;

        Ok(())
    }

    /// Notify MempoolService to clean up in-memory pools (fire-and-forget).
    fn send_mempool_cleanup(
        &self,
        block: &Arc<IrysBlockHeader>,
        transactions: &BlockTransactions,
    ) -> eyre::Result<()> {
        let commitment_tx_ids: Vec<H256> = transactions
            .commitment_txs
            .iter()
            .map(CommitmentTransaction::id)
            .collect();

        let submit_tx_ids: Vec<H256> = transactions
            .data_txs
            .get(&DataLedger::Submit)
            .map(|txs| txs.iter().map(|tx| tx.id).collect())
            .unwrap_or_default();

        self.service_senders
            .mempool
            .send(MempoolServiceMessage::MigrationCleanup {
                block_height: block.height,
                commitment_tx_ids,
                submit_tx_ids,
            })
            .map_err(|e| eyre::eyre!("Failed to send MigrationCleanup message: {}", e))?;

        Ok(())
    }
}
