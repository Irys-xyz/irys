use crate::chunk_migration_service::ChunkMigrationServiceMessage;
use eyre::{OptionExt as _, ensure};
use irys_database::{db::IrysDatabaseExt as _, insert_commitment_tx, insert_tx_header};
use irys_domain::{BlockIndex, BlockTree, SupplyState, block_index_guard::BlockIndexReadGuard};
use irys_types::{
    DataLedger, DataTransactionHeader, IrysBlockHeader, SealedBlock, SendTraced as _, SystemLedger,
    Traced, app_state::DatabaseProvider,
};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info};

/// Block migration orchestration and DB persistence, called inline by `BlockTreeServiceInner`.
#[derive(Debug)]
pub struct BlockMigrationService {
    db: DatabaseProvider,
    block_index_guard: BlockIndexReadGuard,
    supply_state: Option<Arc<SupplyState>>,
    chunk_size: u64,
    cache: Arc<RwLock<BlockTree>>,
    chunk_migration_sender: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
}

impl BlockMigrationService {
    pub fn new(
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        supply_state: Option<Arc<SupplyState>>,
        chunk_size: u64,
        cache: Arc<RwLock<BlockTree>>,
        chunk_migration_sender: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    ) -> Self {
        Self {
            db,
            block_index_guard,
            supply_state,
            chunk_size,
            cache,
            chunk_migration_sender,
        }
    }

    /// Atomically persists tx metadata (included_height, promoted_height) to the DB.
    ///
    /// For normal blocks: `blocks_to_clear` is empty, `blocks_to_confirm` contains just the tip.
    /// For reorgs: `blocks_to_clear` contains orphaned fork blocks, `blocks_to_confirm` contains
    /// the new canonical fork blocks. Both operations happen in a single DB transaction to
    /// prevent inconsistent metadata state.
    pub fn persist_metadata(
        &self,
        blocks_to_clear: &[Arc<SealedBlock>],
        blocks_to_confirm: &[Arc<SealedBlock>],
    ) -> eyre::Result<()> {
        self.db.update_eyre(|tx| {
            // Phase 1: Clear orphaned metadata
            for block in blocks_to_clear {
                let header = block.header();
                let all_data_tx_ids: Vec<_> = header
                    .data_ledgers
                    .iter()
                    .flat_map(|dl| dl.tx_ids.0.iter())
                    .collect();
                let commitment_ids = header.commitment_tx_ids();

                if !all_data_tx_ids.is_empty() {
                    irys_database::batch_clear_data_tx_metadata(tx, all_data_tx_ids.into_iter())
                        .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                if !commitment_ids.is_empty() {
                    irys_database::batch_clear_commitment_tx_metadata(tx, commitment_ids)
                        .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
            }
            // Phase 2: Write confirmed metadata
            for block in blocks_to_confirm {
                let header = block.header();
                let commitment_ids = header.commitment_tx_ids();
                let height = header.height;

                for dl in &header.data_ledgers {
                    let tx_ids = &dl.tx_ids.0;
                    if tx_ids.is_empty() {
                        continue;
                    }
                    irys_database::batch_set_data_tx_included_height(tx, tx_ids, height)
                        .map_err(|e| eyre::eyre!("{:?}", e))?;

                    // Publish ledger txs also get promoted_height
                    if dl.ledger_id == DataLedger::Publish as u32 {
                        irys_database::batch_set_data_tx_promoted_height(tx, tx_ids, height)
                            .map_err(|e| eyre::eyre!("{:?}", e))?;
                    }
                }
                if !commitment_ids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        commitment_ids,
                        height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
            }
            Ok(())
        })?;

        info!(
            cleared_blocks = blocks_to_clear.len(),
            confirmed_blocks = blocks_to_confirm.len(),
            "Persisted metadata to DB"
        );

        Ok(())
    }

    /// Collects blocks from the cache, validates continuity, persists to DB,
    /// writes to the block index, and notifies downstream services.
    ///
    /// The cache read lock is held only while collecting blocks, then released
    /// before performing DB writes and block index mutations.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %migration_block.block_hash, block.height = migration_block.height))]
    pub fn migrate_blocks(&self, migration_block: &Arc<IrysBlockHeader>) -> eyre::Result<()> {
        debug!("migrating block via BlockMigrationService");

        // Collect blocks under the cache read lock, then release it.
        let prepared = {
            let cache = self.cache.read().expect("block tree lock poisoned");
            let blocks = self.get_blocks_to_migrate(migration_block, &cache)?;
            self.validate_migration_continuity(&blocks)?;

            for block in &blocks {
                let header = block.header();
                eyre::ensure!(
                    header.poa.chunk.is_some(),
                    "poa chunk must be present for block {} at height {}",
                    header.block_hash,
                    header.height
                );
            }

            blocks
        }; // RwLockReadGuard dropped here

        debug!("prepared {} block(s) for migration", prepared.len());

        for sealed_block in &prepared {
            self.persist_block(sealed_block)?;
            if let Some(supply_state) = &self.supply_state {
                let block = sealed_block.header();
                supply_state.add_block_reward(block.height, block.reward_amount)?;
            }
            self.send_chunk_migration(sealed_block)?;
        }

        Ok(())
    }

    /// Validates height continuity and chain linkage across the migration slice.
    fn validate_migration_continuity(
        &self,
        blocks_to_migrate: &[Arc<SealedBlock>],
    ) -> eyre::Result<()> {
        let Some(first) = blocks_to_migrate.first() else {
            return Ok(());
        };
        let first = first.header();

        let block_index = self.block_index_guard.read();
        let latest_indexed = block_index
            .get_latest_item()
            .ok_or_eyre("Block index is empty")?;

        ensure!(
            block_index.latest_height() + 1 == first.height,
            "Height gap detected: block index at height {} ({}), trying to migrate height {} ({})",
            &block_index.latest_height(),
            &latest_indexed.block_hash,
            &first.height,
            &first.block_hash
        );
        ensure!(
            latest_indexed.block_hash == first.previous_block_hash,
            "Chain break detected: migration block ({}) doesn't link to indexed chain ({})",
            &latest_indexed.block_hash,
            &first.previous_block_hash
        );

        for pair in blocks_to_migrate.windows(2) {
            let prev = pair[0].header();
            let curr = pair[1].header();
            ensure!(
                curr.height == prev.height + 1,
                "Height gap in migration slice: block {} at height {}, next block {} at height {}",
                prev.block_hash,
                prev.height,
                curr.block_hash,
                curr.height
            );
            ensure!(
                curr.previous_block_hash == prev.block_hash,
                "Chain break in migration slice: block {} doesn't link to previous block {}",
                curr.block_hash,
                prev.block_hash
            );
        }

        Ok(())
    }

    /// Walks backward from `migration_block` to the block_index head, returning oldest-first.
    fn get_blocks_to_migrate(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
        cache: &BlockTree,
    ) -> eyre::Result<Vec<Arc<SealedBlock>>> {
        let mut blocks_to_migrate = vec![];
        let block_index = self.block_index_guard.read();
        let last_migrated = block_index
            .get_latest_item()
            .ok_or_eyre("must have at least a single item in block index")?;
        let last_migrated_hash = last_migrated.block_hash;
        let mut current_hash = migration_block.block_hash;

        while current_hash != last_migrated_hash {
            let metadata = cache.blocks.get(&current_hash).ok_or_else(|| {
                eyre::eyre!(
                    "block {} not found while collecting blocks for migration",
                    current_hash
                )
            })?;
            let sealed_block = Arc::clone(&metadata.block);
            current_hash = sealed_block.header().previous_block_hash;
            blocks_to_migrate.push(sealed_block);
        }

        blocks_to_migrate.reverse();
        Ok(blocks_to_migrate)
    }

    /// Persists block data and block index in a single atomic DB transaction.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height))]
    fn persist_block(&self, sealed_block: &SealedBlock) -> eyre::Result<()> {
        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let commitment_txs = transactions.get_ledger_system_txs(SystemLedger::Commitment);
        let commitment_tx_ids = header.commitment_tx_ids();
        let block_height = header.height;

        let migrated_block = (**header).clone();

        let chunk_size = self.chunk_size;
        self.db.update_eyre(|tx| {
            for commitment_tx in commitment_txs {
                insert_commitment_tx(tx, commitment_tx)?;
            }

            // Persist tx headers and metadata for all data ledgers in the block
            for dl in &header.data_ledgers {
                let ledger = DataLedger::try_from(dl.ledger_id)
                    .map_err(|_| eyre::eyre!("Unknown ledger_id {}", dl.ledger_id))?;
                let mut ledger_txs = transactions.get_ledger_txs(ledger).to_vec();
                let tx_ids = &dl.tx_ids.0;

                // Publish txs get promoted_height set on their headers
                if ledger == DataLedger::Publish {
                    for data_tx in &mut ledger_txs {
                        if data_tx.promoted_height().is_none() {
                            data_tx.metadata_mut().promoted_height = Some(block_height);
                        }
                    }
                }

                for data_tx in &ledger_txs {
                    insert_tx_header(tx, data_tx)?;
                }

                if !tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_included_height(tx, tx_ids, block_height)
                        .map_err(|e| eyre::eyre!("{:?}", e))?;

                    if ledger == DataLedger::Publish {
                        irys_database::batch_set_data_tx_promoted_height(tx, tx_ids, block_height)
                            .map_err(|e| eyre::eyre!("{:?}", e))?;
                    }
                }
            }

            if !commitment_tx_ids.is_empty() {
                irys_database::batch_set_commitment_tx_included_height(
                    tx,
                    commitment_tx_ids,
                    block_height,
                )
                .map_err(|e| eyre::eyre!("{:?}", e))?;
            }

            irys_database::insert_block_header(tx, &migrated_block)?;

            BlockIndex::push_block(tx, sealed_block, chunk_size)?;

            Ok(())
        })?;

        Ok(())
    }

    /// Notify ChunkMigrationService about the migrated block (fire-and-forget).
    fn send_chunk_migration(&self, sealed_block: &SealedBlock) -> eyre::Result<()> {
        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let mut all_txs_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        for dl in &header.data_ledgers {
            if let Ok(ledger) = DataLedger::try_from(dl.ledger_id) {
                all_txs_map.insert(ledger, transactions.get_ledger_txs(ledger).to_vec());
            }
        }

        self.chunk_migration_sender
            .send_traced(ChunkMigrationServiceMessage::BlockMigrated(
                Arc::clone(header),
                Arc::new(all_txs_map),
            ))
            .map_err(|e| eyre::eyre!("Failed to send BlockMigrated message: {}", e))?;

        Ok(())
    }
}
