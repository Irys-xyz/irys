use crate::{
    block_index_service::BlockIndexServiceMessage,
    chunk_migration_service::ChunkMigrationServiceMessage, services::ServiceSenders,
};
use eyre::{ensure, OptionExt as _};
use irys_database::{db::IrysDatabaseExt as _, insert_commitment_tx, insert_tx_header};
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockTree};
use irys_types::{
    app_state::DatabaseProvider, CommitmentTransaction, DataLedger, DataTransactionHeader,
    IrysBlockHeader, SealedBlock, SystemLedger, H256,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::oneshot;
use tracing::info;

/// Block migration orchestration and DB persistence, called inline by `BlockTreeServiceInner`.
///
/// Migrated transactions are removed from the mempool via anchor-expiry pruning
/// in `prune_pending_txs`, not by an explicit cleanup message.
#[derive(Debug)]
pub struct BlockMigrator {
    db: DatabaseProvider,
    block_index_guard: BlockIndexReadGuard,
}

impl BlockMigrator {
    pub const fn new(db: DatabaseProvider, block_index_guard: BlockIndexReadGuard) -> Self {
        Self {
            db,
            block_index_guard,
        }
    }

    /// Clears `included_height` and `promoted_height` metadata from the DB
    /// for all transactions in the given orphaned blocks.
    ///
    /// Called during a reorg before the new fork's metadata is written and
    /// before `ReorgEvent` is broadcast, ensuring no stale metadata persists.
    pub fn clear_orphaned_metadata(&self, old_fork: &[Arc<IrysBlockHeader>]) -> eyre::Result<()> {
        let mut all_data_tx_ids: Vec<H256> = Vec::new();
        let mut all_commitment_tx_ids: Vec<H256> = Vec::new();

        for block in old_fork {
            let submit_tx_ids = &block.data_ledgers[DataLedger::Submit].tx_ids.0;
            let publish_tx_ids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;
            let commitment_tx_ids = block.get_commitment_ledger_tx_ids();

            all_data_tx_ids.extend(submit_tx_ids.iter().chain(publish_tx_ids.iter()));
            all_commitment_tx_ids.extend(commitment_tx_ids);
        }

        if !all_data_tx_ids.is_empty() {
            self.db.update_eyre(|tx| {
                irys_database::batch_clear_data_tx_metadata(tx, &all_data_tx_ids)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            })?;
        }

        if !all_commitment_tx_ids.is_empty() {
            self.db.update_eyre(|tx| {
                irys_database::batch_clear_commitment_tx_metadata(tx, &all_commitment_tx_ids)
                    .map_err(|e| eyre::eyre!("{:?}", e))
            })?;
        }

        info!(
            blocks = old_fork.len(),
            data_txs = all_data_tx_ids.len(),
            commitment_txs = all_commitment_tx_ids.len(),
            "Cleared orphaned metadata from DB"
        );

        Ok(())
    }

    /// Persists `included_height` and `promoted_height` metadata to the DB
    /// for a confirmed block's transactions.
    ///
    /// Runs at confirmation time (every new tip), before the full block
    /// migration at `migration_depth`. Ensures tx status survives restarts.
    pub fn persist_confirmed_metadata(&self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let block_height = block.height;

        let submit_tx_ids = block.data_ledgers[DataLedger::Submit].tx_ids.0.clone();
        let publish_tx_ids = block.data_ledgers[DataLedger::Publish].tx_ids.0.clone();
        let commitment_tx_ids = block.get_commitment_ledger_tx_ids();

        let all_data_tx_ids: Vec<H256> = submit_tx_ids
            .iter()
            .chain(publish_tx_ids.iter())
            .copied()
            .collect();

        if !all_data_tx_ids.is_empty() || !commitment_tx_ids.is_empty() {
            self.db.update_eyre(|tx| {
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
                if !publish_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_promoted_height(
                        tx,
                        &publish_tx_ids,
                        block_height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                Ok(())
            })?;
        }

        info!(
            block.height = block_height,
            data_txs = all_data_tx_ids.len(),
            commitment_txs = commitment_tx_ids.len(),
            "Persisted confirmed metadata to DB"
        );

        Ok(())
    }

    /// Atomically clears orphaned metadata and writes new fork metadata.
    /// Called during reorgs to ensure DB consistency — if clearing old fork
    /// metadata succeeds but writing new fork metadata fails, the DB would
    /// be left in an inconsistent state. This method prevents that by
    /// performing both operations in a single DB transaction.
    pub fn apply_reorg_metadata(
        &self,
        old_fork: &[Arc<IrysBlockHeader>],
        new_fork: &[Arc<IrysBlockHeader>],
    ) -> eyre::Result<()> {
        // Collect all tx IDs from old fork (for clearing)
        let mut old_data_tx_ids: Vec<H256> = Vec::new();
        let mut old_commitment_tx_ids: Vec<H256> = Vec::new();
        for block in old_fork {
            let submit_tx_ids = &block.data_ledgers[DataLedger::Submit].tx_ids.0;
            let publish_tx_ids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;
            let commitment_tx_ids = block.get_commitment_ledger_tx_ids();
            old_data_tx_ids.extend(submit_tx_ids.iter().chain(publish_tx_ids.iter()));
            old_commitment_tx_ids.extend(commitment_tx_ids);
        }

        // Collect per-block info from new fork (for writing)
        struct NewBlockInfo {
            height: u64,
            all_data_tx_ids: Vec<H256>,
            commitment_tx_ids: Vec<H256>,
            publish_tx_ids: Vec<H256>,
        }
        let new_blocks: Vec<NewBlockInfo> = new_fork
            .iter()
            .map(|block| {
                let submit_tx_ids = block.data_ledgers[DataLedger::Submit].tx_ids.0.clone();
                let publish_tx_ids = block.data_ledgers[DataLedger::Publish].tx_ids.0.clone();
                let commitment_tx_ids = block.get_commitment_ledger_tx_ids();
                let all_data_tx_ids: Vec<H256> = submit_tx_ids
                    .iter()
                    .chain(publish_tx_ids.iter())
                    .copied()
                    .collect();
                NewBlockInfo {
                    height: block.height,
                    all_data_tx_ids,
                    commitment_tx_ids,
                    publish_tx_ids,
                }
            })
            .collect();

        // Single atomic DB transaction for both clear + write
        self.db.update_eyre(|tx| {
            // Clear old fork metadata
            if !old_data_tx_ids.is_empty() {
                irys_database::batch_clear_data_tx_metadata(tx, &old_data_tx_ids)
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
            }
            if !old_commitment_tx_ids.is_empty() {
                irys_database::batch_clear_commitment_tx_metadata(tx, &old_commitment_tx_ids)
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
            }
            // Write new fork metadata
            for info in &new_blocks {
                if !info.all_data_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_included_height(
                        tx,
                        &info.all_data_tx_ids,
                        info.height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                if !info.commitment_tx_ids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        &info.commitment_tx_ids,
                        info.height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
                if !info.publish_tx_ids.is_empty() {
                    irys_database::batch_set_data_tx_promoted_height(
                        tx,
                        &info.publish_tx_ids,
                        info.height,
                    )
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
                }
            }
            Ok(())
        })?;

        info!(
            old_blocks = old_fork.len(),
            new_blocks = new_fork.len(),
            "Atomically applied reorg metadata to DB"
        );

        Ok(())
    }

    /// Validates continuity and collects sealed blocks from the cache.
    ///
    /// Separated from [`Self::process_migration`] so the caller can drop the
    /// non-`Send` `RwLockReadGuard<BlockTree>` before awaiting.
    pub fn prepare_migration(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
        cache: &BlockTree,
    ) -> eyre::Result<Vec<Arc<SealedBlock>>> {
        let blocks = self.get_blocks_to_migrate(migration_block, cache)?;
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

        Ok(blocks)
    }

    /// Persists prepared blocks to DB and notifies downstream services.
    #[tracing::instrument(level = "trace", skip_all, fields(count = prepared.len()))]
    pub async fn process_migration(
        &self,
        prepared: Vec<Arc<SealedBlock>>,
        service_senders: &ServiceSenders,
    ) -> eyre::Result<()> {
        for sealed_block in prepared {
            self.persist_block_to_db(&sealed_block)?;
            self.send_block_index_migration(&sealed_block, service_senders)
                .await?;
            self.send_chunk_migration(&sealed_block, service_senders)?;
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

    /// Persists all block data to DB in a single atomic transaction.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height))]
    fn persist_block_to_db(&self, sealed_block: &SealedBlock) -> eyre::Result<()> {
        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let commitment_txs = transactions.get_ledger_system_txs(SystemLedger::Commitment);
        let commitment_tx_ids: Vec<H256> = commitment_txs
            .iter()
            .map(CommitmentTransaction::id)
            .collect();

        let submit_txs = transactions.get_ledger_txs(DataLedger::Submit);
        let submit_tx_ids: Vec<H256> = submit_txs.iter().map(|tx| tx.id).collect();

        let mut publish_txs = transactions.get_ledger_txs(DataLedger::Publish).to_vec();
        let publish_tx_ids: Vec<H256> = publish_txs.iter().map(|tx| tx.id).collect();

        let block_height = header.height;

        let all_data_tx_ids: Vec<H256> = submit_tx_ids
            .iter()
            .chain(publish_tx_ids.iter())
            .copied()
            .collect();

        let migrated_block = (**header).clone();

        self.db.update_eyre(|tx| {
            for commitment_tx in commitment_txs {
                insert_commitment_tx(tx, commitment_tx)?;
            }
            for header in submit_txs {
                insert_tx_header(tx, header)?;
            }
            for header in &mut publish_txs {
                if header.promoted_height().is_none() {
                    header.metadata_mut().promoted_height = Some(block_height);
                }
                insert_tx_header(tx, header)?;
            }

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
            if !publish_tx_ids.is_empty() {
                irys_database::batch_set_data_tx_promoted_height(tx, &publish_tx_ids, block_height)
                    .map_err(|e| eyre::eyre!("{:?}", e))?;
            }

            irys_database::insert_block_header(tx, &migrated_block)?;

            Ok(())
        })?;

        Ok(())
    }

    /// Send migration notification to BlockIndexService (synchronous -- waits for response).
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height))]
    async fn send_block_index_migration(
        &self,
        sealed_block: &SealedBlock,
        senders: &ServiceSenders,
    ) -> eyre::Result<()> {
        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let mut all_txs = Vec::new();
        all_txs.extend_from_slice(transactions.get_ledger_txs(DataLedger::Publish));
        all_txs.extend_from_slice(transactions.get_ledger_txs(DataLedger::Submit));

        info!(
            "Migrating to block_index - hash: {} height: {}",
            &header.block_hash, &header.height
        );

        let (tx, rx) = oneshot::channel();
        senders
            .block_index
            .send(BlockIndexServiceMessage::MigrateBlock {
                block_header: Arc::clone(header),
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
        sealed_block: &SealedBlock,
        senders: &ServiceSenders,
    ) -> eyre::Result<()> {
        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let mut all_txs_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        all_txs_map.insert(
            DataLedger::Submit,
            transactions.get_ledger_txs(DataLedger::Submit).to_vec(),
        );
        all_txs_map.insert(
            DataLedger::Publish,
            transactions.get_ledger_txs(DataLedger::Publish).to_vec(),
        );

        senders
            .chunk_migration
            .send(ChunkMigrationServiceMessage::BlockMigrated(
                Arc::clone(header),
                Arc::new(all_txs_map),
            ))
            .map_err(|e| eyre::eyre!("Failed to send BlockMigrated message: {}", e))?;

        Ok(())
    }
}
