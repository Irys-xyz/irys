use crate::{
    block_index_service::BlockIndexServiceMessage,
    chunk_migration_service::ChunkMigrationServiceMessage, services::ServiceSenders,
};
use eyre::{ensure, OptionExt as _};
use irys_database::{db::IrysDatabaseExt as _, insert_commitment_tx, insert_tx_header};
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockTree};
use irys_types::{
    app_state::DatabaseProvider, BlockTransactions, CommitmentTransaction, DataLedger,
    DataTransactionHeader, IrysBlockHeader, H256,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::oneshot;
use tracing::{error, info};

/// Plain struct that owns block migration orchestration and DB persistence logic.
///
/// Unlike the old `BlockMigrationService` actor, this struct has no message loop,
/// no shutdown handling, and no spawn. It is called inline by `BlockTreeServiceInner`.
///
/// Responsibilities:
/// - Validates migration continuity (height + chain linkage)
/// - Collects blocks to migrate by walking the block tree
/// - Persists all transaction data and block headers to the database
/// - Notifies BlockIndexService (sync) and ChunkMigrationService (fire-and-forget)
/// - Sends cleanup message to MempoolService (fire-and-forget)
#[derive(Debug)]
pub struct BlockMigrator {
    db: DatabaseProvider,
    block_index_guard: BlockIndexReadGuard,
}

impl BlockMigrator {
    /// Creates a new `BlockMigrator` with the given database provider and block index guard.
    pub const fn new(db: DatabaseProvider, block_index_guard: BlockIndexReadGuard) -> Self {
        Self {
            db,
            block_index_guard,
        }
    }

    /// Synchronous preparation phase: validates continuity, collects blocks and their
    /// transactions from the cache. Returns `(block, transactions)` pairs in oldest-first
    /// order, ready for processing without further cache access.
    ///
    /// This is separated from [`Self::process_migration`] so that the caller can hold a
    /// `RwLockReadGuard<BlockTree>` for just this call, drop the guard, and then call
    /// the async `process_migration` without the non-`Send` guard crossing an await.
    pub fn prepare_migration(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
        cache: &BlockTree,
    ) -> eyre::Result<Vec<(Arc<IrysBlockHeader>, BlockTransactions)>> {
        // 1. Collect blocks to migrate (oldest first)
        let blocks_to_migrate = self.get_blocks_to_migrate(migration_block, cache)?;

        // 2. Validate migration continuity
        self.validate_migration_continuity(&blocks_to_migrate)?;

        // 3. Extract transactions and verify POA chunks for each block
        let mut prepared = Vec::with_capacity(blocks_to_migrate.len());
        for block_to_migrate in blocks_to_migrate {
            let transactions = cache
                .blocks
                .get(&block_to_migrate.block_hash)
                .map(|meta| meta.transactions.clone())
                .ok_or_else(|| {
                    eyre::eyre!(
                        "missing cache entry for block {} during block migration",
                        block_to_migrate.block_hash
                    )
                })?;

            eyre::ensure!(
                block_to_migrate.poa.chunk.is_some(),
                "poa chunk must be present for block {} at height {}",
                block_to_migrate.block_hash,
                block_to_migrate.height
            );

            prepared.push((block_to_migrate, transactions));
        }

        Ok(prepared)
    }

    /// Async processing phase: persists each prepared block to DB and notifies downstream
    /// services. Does not access the block tree cache.
    ///
    /// Call [`Self::prepare_migration`] first to obtain the `prepared` list.
    #[tracing::instrument(level = "trace", skip_all, fields(count = prepared.len()))]
    pub async fn process_migration(
        &self,
        prepared: Vec<(Arc<IrysBlockHeader>, BlockTransactions)>,
        service_senders: &ServiceSenders,
    ) -> eyre::Result<()> {
        for (block_to_migrate, transactions) in prepared {
            // Persist all transactions and block header to DB
            self.persist_block_to_db(&block_to_migrate, &transactions)?;

            // Notify BlockIndexService (synchronous -- wait for completion)
            self.send_block_index_migration(&block_to_migrate, &transactions, service_senders)
                .await?;

            // Notify ChunkMigrationService (fire-and-forget)
            self.send_chunk_migration(&block_to_migrate, &transactions, service_senders)?;
        }

        Ok(())
    }

    /// Ensures blocks can be migrated by verifying height continuity and chain linkage.
    fn validate_migration_continuity(
        &self,
        blocks_to_migrate: &[Arc<IrysBlockHeader>],
    ) -> eyre::Result<()> {
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
        cache: &BlockTree,
    ) -> eyre::Result<Vec<Arc<IrysBlockHeader>>> {
        let mut blocks_to_migrate = vec![];

        // Get the last migrated block hash
        let block_index = self.block_index_guard.read();
        let last_migrated = block_index
            .get_latest_item()
            .ok_or_eyre("must have at least a single item in block index")?;
        let last_migrated_hash = last_migrated.block_hash;

        // Walk backwards from the given block to the last migrated block
        let mut current_hash = migration_block.block_hash;

        while current_hash != last_migrated_hash {
            let current_block = cache.get_block(&current_hash).ok_or_else(|| {
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

    /// Send migration notification to BlockIndexService (synchronous -- waits for response).
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = block.height))]
    async fn send_block_index_migration(
        &self,
        block: &Arc<IrysBlockHeader>,
        transactions: &BlockTransactions,
        senders: &ServiceSenders,
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
        senders
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
        senders: &ServiceSenders,
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

        senders
            .chunk_migration
            .send(ChunkMigrationServiceMessage::BlockMigrated(
                Arc::clone(block),
                Arc::new(all_txs_map),
            ))
            .map_err(|e| eyre::eyre!("Failed to send BlockMigrated message: {}", e))?;

        Ok(())
    }
}
