use crate::chunk_migration_service::ChunkMigrationServiceMessage;
use eyre::{OptionExt as _, ensure};
use irys_database::{
    block_header_by_hash, db::IrysDatabaseExt as _, insert_commitment_tx, insert_tx_header,
    tx_header_by_txid,
};
use irys_domain::{
    BlockIndex, BlockTree, ChunkType, StorageModule, StorageModulesReadGuard, SupplyState,
    block_index_guard::BlockIndexReadGuard, get_overlapped_storage_modules,
};
use irys_storage::ii;
use irys_types::{
    DataLedger, DataTransactionHeader, H256, IrysBlockHeader, LedgerChunkOffset, LedgerChunkRange,
    PartitionChunkOffset, SealedBlock, SendTraced as _, SystemLedger, Traced, U256,
    app_state::DatabaseProvider,
};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

/// Block migration orchestration and DB persistence, called inline by `BlockTreeServiceInner`.
#[derive(Debug)]
pub struct BlockMigrationService {
    db: DatabaseProvider,
    block_index_guard: BlockIndexReadGuard,
    supply_state: Option<Arc<SupplyState>>,
    chunk_size: u64,
    cache: Arc<RwLock<BlockTree>>,
    chunk_migration_sender: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    storage_modules_guard: Option<StorageModulesReadGuard>,
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
            storage_modules_guard: None,
        }
    }

    pub fn set_storage_modules_guard(&mut self, guard: StorageModulesReadGuard) {
        self.storage_modules_guard = Some(guard);
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

    /// Rolls back blocks migrated on a minority fork during network partition recovery.
    ///
    /// Unassigns storage module offsets, truncates the block index, and rolls back
    /// the supply state to the fork parent height.
    pub fn recover_from_network_partition(&self, fork_parent_height: u64) -> eyre::Result<()> {
        let storage_modules_guard = self.storage_modules_guard.as_ref().ok_or_else(|| {
            eyre::eyre!("storage_modules_guard not set during partition recovery")
        })?;

        let block_index = self.block_index_guard.read();
        let latest = block_index.latest_height();
        if fork_parent_height >= latest {
            return Ok(());
        }

        let rollback_count = latest - fork_parent_height;
        warn!(
            fork_parent_height,
            latest_indexed = latest,
            rollback_count,
            "recovering from network partition"
        );

        // Phase 1: Collect per-block chunk ranges, tx data_roots, and reward amounts
        // while the block index is still intact.
        struct BlockRollbackInfo {
            ledger_ranges: Vec<(DataLedger, LedgerChunkRange)>,
            data_roots: Vec<H256>,
            reward_amount: U256,
        }

        let mut rollback_infos = Vec::with_capacity(rollback_count as usize);
        for h in (fork_parent_height + 1)..=latest {
            let item = block_index
                .get_item(h)
                .ok_or_else(|| eyre::eyre!("missing block index entry at height {h}"))?;

            let prev_item = block_index
                .get_item(h - 1)
                .ok_or_else(|| eyre::eyre!("missing block index entry at height {}", h - 1))?;

            let header = self
                .db
                .view_eyre(|tx| block_header_by_hash(tx, &item.block_hash, false))?
                .ok_or_else(|| eyre::eyre!("missing block header for {}", item.block_hash))?;

            let mut ledger_ranges = Vec::new();
            for ledger_item in &item.ledgers {
                let prev_total = prev_item
                    .ledgers
                    .iter()
                    .find(|li| li.ledger == ledger_item.ledger)
                    .map(|li| li.total_chunks)
                    .unwrap_or(0);

                if ledger_item.total_chunks > prev_total {
                    let range = LedgerChunkRange(ii(
                        LedgerChunkOffset::from(prev_total),
                        LedgerChunkOffset::from(ledger_item.total_chunks - 1),
                    ));
                    ledger_ranges.push((ledger_item.ledger, range));
                }
            }

            // Collect data_roots from orphaned block's transactions
            let mut data_roots = Vec::new();
            let tx_id_map = header.get_data_ledger_tx_ids();
            for tx_ids in tx_id_map.values() {
                for txid in tx_ids {
                    if let Ok(Some(tx_header)) = self.db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                    {
                        data_roots.push(tx_header.data_root);
                    }
                }
            }

            rollback_infos.push(BlockRollbackInfo {
                ledger_ranges,
                data_roots,
                reward_amount: header.reward_amount,
            });
        }

        // Phase 2: Unassign storage module offsets and clear orphaned index entries
        for info in &rollback_infos {
            for (ledger, range) in &info.ledger_ranges {
                let modules = get_overlapped_storage_modules(storage_modules_guard, *ledger, range);

                for module in modules {
                    let partition_range = match module.make_range_partition_relative(*range) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };

                    // Clear offset index entries and data_root mappings for orphaned txs
                    for offset in *partition_range.start()..=*partition_range.end() {
                        let part_offset = PartitionChunkOffset::from(offset);
                        let (_, submodule) = module.get_submodule_for_offset(part_offset)?;
                        submodule.db.update_eyre(|tx| {
                            irys_database::submodule::add_tx_path_hash_to_offset_index(
                                tx,
                                part_offset,
                                None,
                            )?;
                            irys_database::submodule::add_data_path_hash_to_offset_index(
                                tx,
                                part_offset,
                                None,
                            )?;
                            // Clear DataRootInfosByDataRoot for orphaned txs' data_roots
                            for data_root in &info.data_roots {
                                irys_database::submodule::set_data_root_infos_for_data_root(
                                    tx,
                                    *data_root,
                                    irys_database::submodule::tables::DataRootInfos(Vec::new()),
                                )?;
                            }
                            Ok(())
                        })?;
                    }

                    // Mark offsets as Uninitialized (not Entropy — on-disk bytes are packed data)
                    {
                        let mut intervals = module.intervals().write().unwrap();
                        for offset in *partition_range.start()..=*partition_range.end() {
                            let part_offset = PartitionChunkOffset::from(offset);
                            StorageModule::cut_then_insert_interval_if_touching(
                                &mut intervals,
                                part_offset,
                                ChunkType::Uninitialized,
                            );
                        }
                    }
                    module.write_intervals_to_submodules()?;
                }
            }
        }

        // Phase 3: Truncate block index
        block_index.truncate_to_height(fork_parent_height)?;

        // Phase 4: Roll back supply state
        if let Some(supply_state) = &self.supply_state {
            let reward_sum: U256 = rollback_infos.iter().fold(U256::zero(), |acc, info| {
                acc.saturating_add(info.reward_amount)
            });
            supply_state.rollback_reward(fork_parent_height, reward_sum);
        }

        info!(
            fork_parent_height,
            blocks_rolled_back = rollback_count,
            "network partition recovery complete"
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
