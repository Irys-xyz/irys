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

/// Write `included_height` for term ledgers and `promoted_height` for the
/// Publish ledger from a single block's `data_ledgers` slice.
///
/// NOTE: Keep the term-ledger pass first — same-block Submit→Publish
/// promotions require `included_height` to exist before
/// `set_data_tx_promoted_height` runs for the same tx_id.
fn write_data_ledger_metadata(
    tx: &(impl reth_db::transaction::DbTxMut + reth_db::transaction::DbTx),
    data_ledgers: &[irys_types::DataTransactionLedger],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    // Term ledgers (Submit / OneYear / ThirtyDay): set included_height.
    for dl in data_ledgers
        .iter()
        .filter(|dl| dl.ledger_id != DataLedger::Publish as u32)
    {
        let tx_ids = &dl.tx_ids.0;
        if !tx_ids.is_empty() {
            irys_database::batch_set_data_tx_included_height(tx, tx_ids, height)?;
        }
    }
    // Publish ledger: set promoted_height only — never touch included_height.
    for dl in data_ledgers
        .iter()
        .filter(|dl| dl.ledger_id == DataLedger::Publish as u32)
    {
        let tx_ids = &dl.tx_ids.0;
        if !tx_ids.is_empty() {
            irys_database::batch_set_data_tx_promoted_height(tx, tx_ids, height)?;
        }
    }
    Ok(())
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
    /// Atomically persists tx metadata (included_height, promoted_height) and
    /// keeps the `CachedDataRoots.block_set` hint consistent with the canonical
    /// chain.
    ///
    /// For normal blocks: `blocks_to_clear` is empty, `blocks_to_confirm` contains just the tip.
    /// For reorgs: `blocks_to_clear` contains orphaned fork blocks, `blocks_to_confirm` contains
    /// the new canonical fork blocks. Both operations happen in a single DB transaction to
    /// prevent inconsistent metadata state.
    ///
    /// Invariant established at every commit boundary: if a Submit-ledger tx
    /// in `blocks_to_confirm` has a [`CachedDataRoot`] entry, its `block_set`
    /// contains the confirming block hash and `expiry_height` is `None`.
    /// Orphan block hashes from `blocks_to_clear` are scrubbed from any
    /// `block_set` that retained them so the hint stays meaningful across
    /// reorgs.
    ///
    /// **`block_set` writer model.**  `block_set` is a hint, not consensus
    /// state — canonical truth lives in `IrysDataTxMetadata` +
    /// `MigratedBlockHashes`.  Two writers maintain the hint:
    ///   - The mempool `BlockConfirmed` path (`cache_data_root(_, Some(block))`)
    ///     fires *before* migration and serves as a forward-fill: it records
    ///     the confirming `block_hash` so when chunks arrive *after* block
    ///     confirmation, [`try_generate_ingress_proof_for_root`] finds a
    ///     populated `block_set` and proceeds.  It may fabricate a CDR row
    ///     for a data_root the node has no chunks for yet; the entry is
    ///     harmless until either chunks arrive or the prune pass evicts it.
    ///   - `persist_metadata` (this function) runs at migration time and is
    ///     the authoritative writer for canonical state changes: Phase 1
    ///     scrubs orphan block_hashes from any retained `block_set`, and
    ///     Phase 3's `update_data_root_block_set` is update-only — it never
    ///     fabricates, because by migration time any data_root the node
    ///     cares about already has a CDR (either chunks have arrived, or
    ///     the mempool BlockConfirmed path put one there).
    ///
    /// A stale `BlockConfirmed` arriving after a reorg can re-add an orphan
    /// block_hash to `block_set`; that is tolerated because consumers treat
    /// `block_set` as a hint and re-verify canonicality through
    /// `find_canonical_ledger_range` before acting.
    ///
    /// Reorg invariant: if a Publish-promotion remains canonical, the
    /// underlying Submit inclusion must also be present in `blocks_to_confirm`
    /// — otherwise the Publish block itself would be in `blocks_to_clear`.
    /// Therefore Phase 1 always deletes orphaned term-ledger metadata rows
    /// outright (the `{None, Some}` state is semantically illegal:
    /// `promoted_height` requires a prior `included_height`).  Any matching
    /// Publish `clear_data_tx_promoted_height` becomes a no-op on the
    /// already-deleted row.  Phase 2 then recreates rows for txs that are
    /// re-canonical on the new fork, restoring `promoted_height` in the same
    /// transaction when the Publish block is also re-included.
    pub fn persist_metadata(
        &self,
        blocks_to_clear: &[Arc<SealedBlock>],
        blocks_to_confirm: &[Arc<SealedBlock>],
    ) -> eyre::Result<()> {
        // Phase 1 scrub and Phase 3 append both iterate
        // `block.transactions().get_ledger_txs(DataLedger::Submit)`. That call
        // returns `&[]` for a SealedBlock whose body has been stripped, even
        // if `header.data_ledgers[Submit].tx_ids` is populated — silently
        // skipping every `block_set` mutation.  The in-memory block-tree path
        // always carries full transactions, but future code paths (e.g.
        // blocks hydrated from disk without their body) could regress this
        // without any error.  Surface the divergence in every build so the
        // caller fails loudly rather than committing partial state.
        for block in blocks_to_clear.iter().chain(blocks_to_confirm.iter()) {
            let header_submit_count = block
                .header()
                .data_ledgers
                .iter()
                .find(|dl| dl.ledger_id == DataLedger::Submit as u32)
                .map_or(0, |dl| dl.tx_ids.0.len());
            let body_submit_count = block
                .transactions()
                .get_ledger_txs(DataLedger::Submit)
                .len();
            ensure!(
                header_submit_count == body_submit_count,
                "SealedBlock {} has header/body Submit-tx count mismatch ({} vs {}); transactions appear stripped — block_set mutations would silently no-op",
                block.header().block_hash,
                header_submit_count,
                body_submit_count,
            );
        }

        self.db.update_eyre(|tx| {
            // Phase 1: Clear orphaned tx metadata, and scrub the orphaned
            // block_hash from any CachedDataRoot.block_set that retained it.
            //
            // Per-ledger semantics:
            //   - Term-ledger (Submit, OneYear, ThirtyDay): delete the row.
            //     The reorg invariant guarantees the matching Publish block is
            //     also in `blocks_to_clear` (if the tx was promoted at all),
            //     so either iteration order ends with the row deleted.
            //   - Publish ledger: clear `promoted_height` only.  A Submit-confirmed
            //     `included_height` on the same tx must not be disturbed
            //     (Publish-only orphan: Submit stays canonical, row kept as
            //     `{Some, None}`).
            for block in blocks_to_clear {
                let header = block.header();
                let orphan_block_hash = header.block_hash;
                let commitment_ids = header.commitment_tx_ids();

                for dl in &header.data_ledgers {
                    let tx_ids = &dl.tx_ids.0;
                    if tx_ids.is_empty() {
                        continue;
                    }
                    if dl.ledger_id == DataLedger::Publish as u32 {
                        irys_database::batch_clear_data_tx_promoted_height(tx, tx_ids)?;
                    } else {
                        // Term ledger (Submit / OneYear / ThirtyDay)
                        irys_database::batch_clear_data_tx_included_height(tx, tx_ids)?;
                    }
                }
                if !commitment_ids.is_empty() {
                    irys_database::batch_clear_commitment_tx_metadata(tx, commitment_ids)?;
                }

                for submit_tx in block.transactions().get_ledger_txs(DataLedger::Submit) {
                    irys_database::remove_data_root_block_set_entry(
                        tx,
                        submit_tx.data_root,
                        orphan_block_hash,
                    )?;
                }
            }
            // Phase 2: Write confirmed metadata.
            // Phase 3: Backstop the CachedDataRoot.block_set invariant for the
            //          canonical Submit-ledger txs we just confirmed (update-only,
            //          does not create cache entries for data_roots the node
            //          never tracked chunks for).
            //
            // `included_height` ↔ term ledgers (Submit, OneYear, ThirtyDay) only.
            // `promoted_height` ↔ Publish ledger only.
            // Writing `included_height` for Publish txs would overwrite the
            // Submit-block height that was set when the tx first entered the
            // Submit ledger, violating the "first included in Submit" invariant.
            // Phase 3 reads tx bodies (not the metadata written in Phase 2), so
            // it has no ordering dependency on Phase 2 writes and is folded into
            // the same outer loop.
            for block in blocks_to_confirm {
                let header = block.header();
                let commitment_ids = header.commitment_tx_ids();
                let height = header.height;
                let block_hash = header.block_hash;

                write_data_ledger_metadata(tx, &header.data_ledgers, height)?;
                if !commitment_ids.is_empty() {
                    irys_database::batch_set_commitment_tx_included_height(
                        tx,
                        commitment_ids,
                        height,
                    )?;
                }

                // Phase 3: append `block_hash` to each Submit-tx's CDR.block_set.
                for submit_tx in block.transactions().get_ledger_txs(DataLedger::Submit) {
                    irys_database::update_data_root_block_set(tx, submit_tx.data_root, block_hash)?;
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
            }

            write_data_ledger_metadata(tx, &header.data_ledgers, block_height)?;

            if !commitment_tx_ids.is_empty() {
                irys_database::batch_set_commitment_tx_included_height(
                    tx,
                    commitment_tx_ids,
                    block_height,
                )?;
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

#[cfg(test)]
mod tests {
    //! Focused unit tests for `BlockMigrationService::persist_metadata` —
    //! the authoritative-writer entry point for
    //! `CachedDataRoot.block_set` and `IrysDataTxMetadata.included_height`.
    //!
    //! The function only touches `self.db`; the other service fields
    //! (block_index_guard, cache, chunk_migration_sender, supply_state) are
    //! satisfied with cheap placeholders.
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, cache_data_root, open_or_create_db,
        tables::{CachedDataRoots, IrysTables},
    };
    use irys_domain::BlockTree;
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::{
        BlockTransactions, ConsensusConfig, DataTransactionHeader, DataTransactionHeaderV1,
        DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, H256, H256List,
        IrysBlockHeader, U256,
    };
    use reth_db::Database as _;
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    /// Build a `BlockMigrationService` whose only meaningful field is `db`.
    /// All other dependencies are placeholders sufficient to construct but
    /// not exercised by `persist_metadata`.
    fn make_service(
        db: DatabaseProvider,
    ) -> (
        BlockMigrationService,
        irys_testing_utils::utils::tempfile::TempDir,
    ) {
        // BlockIndex needs a writable DB; reuse the test DB.
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard = BlockIndexReadGuard::new(block_index);

        // BlockTree needs a genesis to seal; consensus testing config.
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = H256::zero();
        genesis.cumulative_diff = U256::from(0);
        genesis.test_sign();
        let tree = BlockTree::new(&genesis, ConsensusConfig::testing());
        let cache = Arc::new(RwLock::new(tree));

        let (chunk_migration_sender, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Return a dummy tempdir alongside so callers can keep it alive if
        // they want; in practice the test owns its own tempdir for the DB.
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let svc = BlockMigrationService::new(
            db,
            block_index_guard,
            None, // supply_state — unused by persist_metadata
            ConsensusConfig::testing().chunk_size,
            cache,
            chunk_migration_sender,
        );
        (svc, tmp)
    }

    fn open_db() -> eyre::Result<(
        DatabaseProvider,
        irys_testing_utils::utils::tempfile::TempDir,
    )> {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let env = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        Ok((DatabaseProvider(Arc::new(env)), tmp))
    }

    /// Build a signed block header with the given Submit-ledger `tx_ids`
    /// and `total_chunks`.
    fn make_block_header(
        height: u64,
        previous_block_hash: H256,
        submit_total_chunks: u64,
        submit_tx_ids: Vec<H256>,
    ) -> IrysBlockHeader {
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.previous_block_hash = previous_block_hash;
        header.cumulative_diff = U256::from(height);
        let submit = &mut header.data_ledgers[DataLedger::Submit as usize];
        submit.total_chunks = submit_total_chunks;
        submit.tx_ids = H256List(submit_tx_ids);
        header.test_sign();
        header
    }

    /// Wrap a header in a `SealedBlock` whose body carries matching
    /// `DataTransactionHeader` entries for each Submit tx_id (so
    /// `block.transactions().get_ledger_txs(Submit)` returns non-empty).
    fn make_sealed_with_submit_txs(
        header: IrysBlockHeader,
        submit_data_roots: &[(H256, H256)], // (tx_id, data_root)
    ) -> Arc<SealedBlock> {
        let data_txs: Vec<DataTransactionHeader> = submit_data_roots
            .iter()
            .map(|(tx_id, data_root)| {
                DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        id: *tx_id,
                        data_root: *data_root,
                        ledger_id: DataLedger::Submit as u32,
                        ..Default::default()
                    },
                    metadata: DataTransactionMetadata::new(),
                })
            })
            .collect();
        let mut data_txs_map = HashMap::new();
        data_txs_map.insert(DataLedger::Submit, data_txs);
        let body = BlockTransactions {
            data_txs: data_txs_map,
            ..Default::default()
        };
        Arc::new(SealedBlock::new_unchecked(Arc::new(header), body))
    }

    /// Seed a CachedDataRoot via the normal `cache_data_root` path, then
    /// stamp specific `block_set` / `expiry_height` values.
    fn seed_cdr(
        db: &DatabaseProvider,
        data_root: H256,
        tx_id: H256,
        block_set: Vec<H256>,
        expiry: Option<u64>,
    ) {
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root,
                ledger_id: DataLedger::Submit as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|tx| -> eyre::Result<()> {
            cache_data_root(tx, &tx_header, None)?;
            let mut cdr = tx
                .get::<CachedDataRoots>(data_root)?
                .expect("seed_cdr: cache_data_root must produce an entry");
            cdr.block_set = block_set;
            cdr.expiry_height = expiry;
            tx.put::<CachedDataRoots>(data_root, cdr)?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    }

    fn read_cdr(
        db: &DatabaseProvider,
        data_root: H256,
    ) -> Option<irys_database::db_cache::CachedDataRoot> {
        db.view(|tx| tx.get::<CachedDataRoots>(data_root))
            .unwrap()
            .unwrap()
    }

    /// Phase 3 normal advance: an existing CDR for a Submit tx in
    /// `blocks_to_confirm` gets the new tip's `block_hash` appended and
    /// `expiry_height` cleared.
    #[tokio::test]
    async fn phase3_appends_tip_hash_and_clears_expiry_on_existing_cdr() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();
        // Pre-confirmation state: expiry set, block_set empty.
        seed_cdr(&db, data_root, tx_id, vec![], Some(99));

        let tip = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let tip_hash = tip.block_hash;
        let tip_sealed = make_sealed_with_submit_txs(tip, &[(tx_id, data_root)]);

        svc.persist_metadata(&[], std::slice::from_ref(&tip_sealed))?;

        let cdr = read_cdr(&db, data_root).expect("CDR present");
        assert_eq!(
            cdr.block_set,
            vec![tip_hash],
            "tip hash appended to block_set"
        );
        assert!(
            cdr.expiry_height.is_none(),
            "expiry cleared by Phase 3 update_data_root_block_set"
        );
        Ok(())
    }

    /// Phase 3 update-only contract: a Submit tx in `blocks_to_confirm`
    /// with no corresponding CDR row produces no CDR write.  Mirrors
    /// `update_data_root_block_set`'s "must not fabricate" doc.
    #[tokio::test]
    async fn phase3_does_not_fabricate_cdr_when_missing() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();
        // No seed_cdr call — no CDR for this data_root.

        let tip = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let tip_sealed = make_sealed_with_submit_txs(tip, &[(tx_id, data_root)]);

        svc.persist_metadata(&[], std::slice::from_ref(&tip_sealed))?;

        assert!(
            read_cdr(&db, data_root).is_none(),
            "Phase 3 must not create a CDR for a data_root the node never tracked"
        );
        Ok(())
    }

    /// Phase 1 scrub on reorg: an existing CDR carrying the orphaned
    /// block's hash has that hash removed.  `expiry_height` is left
    /// untouched per the helper's docblock — mempool re-anchor restores it.
    #[tokio::test]
    async fn phase1_scrubs_orphan_block_hash_from_cdr() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();
        let other_canonical_hash = H256::random();
        let orphan_tip = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let orphan_hash = orphan_tip.block_hash;

        // CDR currently records the orphaned tip alongside another canonical
        // hash; expiry already cleared (post-confirmation).
        seed_cdr(
            &db,
            data_root,
            tx_id,
            vec![orphan_hash, other_canonical_hash],
            None,
        );

        let orphan_sealed = make_sealed_with_submit_txs(orphan_tip, &[(tx_id, data_root)]);

        svc.persist_metadata(std::slice::from_ref(&orphan_sealed), &[])?;

        let cdr = read_cdr(&db, data_root).expect("CDR present");
        assert_eq!(
            cdr.block_set,
            vec![other_canonical_hash],
            "orphan hash scrubbed; non-orphan hash retained"
        );
        assert!(
            cdr.expiry_height.is_none(),
            "expiry_height intentionally left untouched per helper docblock"
        );
        Ok(())
    }

    /// Build a block header that carries a single tx_id in the given ledger.
    /// For ledgers not present in `new_mock_header()` (OneYear, ThirtyDay),
    /// appends the appropriate `DataTransactionLedger` entry.
    fn make_block_header_with_ledger(
        height: u64,
        previous_block_hash: H256,
        ledger: DataLedger,
        tx_id: H256,
    ) -> IrysBlockHeader {
        use irys_types::DataTransactionLedger;
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.previous_block_hash = previous_block_hash;
        header.cumulative_diff = U256::from(height);

        match ledger {
            DataLedger::Publish | DataLedger::Submit => {
                // These ledgers are already present in new_mock_header; just set tx_ids.
                let entry = &mut header.data_ledgers[ledger as usize];
                entry.tx_ids = H256List(vec![tx_id]);
            }
            DataLedger::OneYear | DataLedger::ThirtyDay => {
                header.data_ledgers.push(DataTransactionLedger {
                    ledger_id: ledger.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(vec![tx_id]),
                    total_chunks: 0,
                    expires: Some(10),
                    proofs: None,
                    required_proof_count: None,
                });
            }
        }
        header.test_sign();
        header
    }

    /// Wrap a header in a `SealedBlock` whose body carries a single
    /// `DataTransactionHeader` for `tx_id` in `ledger`.
    fn make_sealed_single_ledger_tx(
        header: IrysBlockHeader,
        ledger: DataLedger,
        tx_id: H256,
    ) -> Arc<SealedBlock> {
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root: H256::random(),
                ledger_id: ledger as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let mut data_txs_map = HashMap::new();
        data_txs_map.insert(ledger, vec![tx_header]);
        let body = BlockTransactions {
            data_txs: data_txs_map,
            ..Default::default()
        };
        Arc::new(SealedBlock::new_unchecked(Arc::new(header), body))
    }

    /// Wrap a header in a `SealedBlock` whose body carries the same tx_id in
    /// both Submit and Publish. This models a same-block promotion.
    fn make_sealed_same_block_submit_publish_promotion(
        mut header: IrysBlockHeader,
        tx_id: H256,
        data_root: H256,
    ) -> Arc<SealedBlock> {
        header.data_ledgers[DataLedger::Publish].tx_ids = H256List(vec![tx_id]);
        header.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx_id]);

        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root,
                // Submit-ledger entries already carry Publish-targeting txs.
                ledger_id: DataLedger::Publish as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });

        let mut data_txs_map = HashMap::new();
        data_txs_map.insert(DataLedger::Publish, vec![tx_header.clone()]);
        data_txs_map.insert(DataLedger::Submit, vec![tx_header]);

        let body = BlockTransactions {
            data_txs: data_txs_map,
            ..Default::default()
        };
        Arc::new(SealedBlock::new_unchecked(Arc::new(header), body))
    }

    fn read_data_tx_metadata(
        db: &DatabaseProvider,
        tx_id: &H256,
    ) -> Option<DataTransactionMetadata> {
        db.view(|tx| irys_database::get_data_tx_metadata(tx, tx_id))
            .unwrap()
            .unwrap()
    }

    // ---- included_height / promoted_height correctness tests ----

    /// Submit→Publish promotion: after confirming in a Submit block at height H,
    /// then a Publish block at height H+N, `included_height` must still be H
    /// and `promoted_height` must be H+N.
    #[tokio::test]
    async fn submit_to_publish_promotion_preserves_included_height() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let submit_height = 1_u64;
        let publish_height = 5_u64;

        // Confirm in Submit ledger at height 1
        let submit_header =
            make_block_header_with_ledger(submit_height, H256::random(), DataLedger::Submit, tx_id);
        let submit_sealed = make_sealed_single_ledger_tx(submit_header, DataLedger::Submit, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&submit_sealed))?;

        // Confirm same tx in Publish ledger at height 5
        let publish_header = make_block_header_with_ledger(
            publish_height,
            H256::random(),
            DataLedger::Publish,
            tx_id,
        );
        let publish_sealed =
            make_sealed_single_ledger_tx(publish_header, DataLedger::Publish, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&publish_sealed))?;

        let meta =
            read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after confirmation");
        assert_eq!(
            meta.included_height,
            Some(submit_height),
            "included_height must stay at Submit-block height, not be overwritten by Publish"
        );
        assert_eq!(
            meta.promoted_height,
            Some(publish_height),
            "promoted_height must be set to Publish-block height"
        );
        Ok(())
    }

    /// Same-block Submit→Publish promotion: the term-ledger metadata write
    /// must happen before the Publish write so `promoted_height` can be set in
    /// the same transaction.
    #[tokio::test]
    async fn same_block_submit_publish_promotion_sets_both_heights() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();
        let height = 11_u64;

        let header = make_block_header(height, H256::random(), 50, vec![tx_id]);
        let sealed = make_sealed_same_block_submit_publish_promotion(header, tx_id, data_root);

        svc.persist_metadata(&[], std::slice::from_ref(&sealed))?;

        let meta =
            read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after confirmation");
        assert_eq!(meta.included_height, Some(height));
        assert_eq!(meta.promoted_height, Some(height));
        Ok(())
    }

    /// `persist_block` uses the same metadata helpers as `persist_metadata`,
    /// so same-block promotions must also write `included_height` before
    /// `promoted_height` during migration.
    #[tokio::test]
    async fn persist_block_same_block_submit_publish_promotion_sets_both_heights()
    -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();

        // Height 0 avoids block-index continuity requirements in this focused
        // unit test while still exercising the metadata write ordering.
        let header = make_block_header(0, H256::zero(), 50, vec![tx_id]);
        let sealed = make_sealed_same_block_submit_publish_promotion(header, tx_id, data_root);

        svc.persist_block(sealed.as_ref())?;

        let meta = read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after migration");
        assert_eq!(meta.included_height, Some(0));
        assert_eq!(meta.promoted_height, Some(0));
        Ok(())
    }

    /// Direct inclusion in a term ledger (no prior Submit):
    /// `included_height` is set, `promoted_height` stays `None`.
    #[rstest::rstest]
    #[case::oneyear(DataLedger::OneYear, 3_u64)]
    #[case::thirtyday(DataLedger::ThirtyDay, 7_u64)]
    #[tokio::test]
    async fn direct_inclusion_sets_included_height(
        #[case] ledger: DataLedger,
        #[case] height: u64,
    ) -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();

        let header = make_block_header_with_ledger(height, H256::random(), ledger, tx_id);
        let sealed = make_sealed_single_ledger_tx(header, ledger, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&sealed))?;

        let meta =
            read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after confirmation");
        assert_eq!(meta.included_height, Some(height));
        assert_eq!(meta.promoted_height, None);
        Ok(())
    }

    /// Phase 1 orphan of Publish promotion only:
    /// tx confirmed in Submit at H, promoted in Publish at H+N.
    /// Orphan the Publish block. `included_height` must stay at H; `promoted_height` cleared.
    #[tokio::test]
    async fn phase1_orphan_publish_only_preserves_submit_included_height() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let submit_height = 2_u64;
        let publish_height = 8_u64;

        // Confirm in Submit at height 2
        let submit_header =
            make_block_header_with_ledger(submit_height, H256::random(), DataLedger::Submit, tx_id);
        let submit_sealed = make_sealed_single_ledger_tx(submit_header, DataLedger::Submit, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&submit_sealed))?;

        // Promote in Publish at height 8
        let publish_header = make_block_header_with_ledger(
            publish_height,
            H256::random(),
            DataLedger::Publish,
            tx_id,
        );
        let publish_sealed =
            make_sealed_single_ledger_tx(publish_header, DataLedger::Publish, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&publish_sealed))?;

        // Verify both are set
        let meta = read_data_tx_metadata(&db, &tx_id).unwrap();
        assert_eq!(meta.included_height, Some(submit_height));
        assert_eq!(meta.promoted_height, Some(publish_height));

        // Orphan the Publish block only
        svc.persist_metadata(std::slice::from_ref(&publish_sealed), &[])?;

        let meta = read_data_tx_metadata(&db, &tx_id)
            .expect("row must still exist: included_height is preserved");
        assert_eq!(
            meta.included_height,
            Some(submit_height),
            "Submit-block included_height must survive Publish-block orphan"
        );
        assert_eq!(
            meta.promoted_height, None,
            "promoted_height must be cleared by Phase 1"
        );
        Ok(())
    }

    /// Phase 1 orphan of Submit confirmation: tx in Submit at H.
    /// Orphan that block. `included_height` cleared, row deleted.
    #[tokio::test]
    async fn phase1_orphan_submit_clears_included_height_and_deletes_row() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let height = 4_u64;

        let header =
            make_block_header_with_ledger(height, H256::random(), DataLedger::Submit, tx_id);
        let sealed = make_sealed_single_ledger_tx(header, DataLedger::Submit, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&sealed))?;

        // Verify it exists
        assert!(read_data_tx_metadata(&db, &tx_id).is_some());

        // Orphan it
        svc.persist_metadata(std::slice::from_ref(&sealed), &[])?;

        assert!(
            read_data_tx_metadata(&db, &tx_id).is_none(),
            "row must be deleted when Submit-block is orphaned"
        );
        Ok(())
    }

    /// Phase 1 orphan of a term-ledger confirmation: tx in `ledger` at H.
    /// Orphan. `included_height` cleared and the row is deleted.
    #[rstest::rstest]
    #[case::oneyear(DataLedger::OneYear, 6_u64)]
    #[case::thirtyday(DataLedger::ThirtyDay, 9_u64)]
    #[tokio::test]
    async fn phase1_orphan_term_ledger_clears_included_height(
        #[case] ledger: DataLedger,
        #[case] height: u64,
    ) -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();

        let header = make_block_header_with_ledger(height, H256::random(), ledger, tx_id);
        let sealed = make_sealed_single_ledger_tx(header, ledger, tx_id);
        svc.persist_metadata(&[], std::slice::from_ref(&sealed))?;

        assert!(read_data_tx_metadata(&db, &tx_id).is_some());

        svc.persist_metadata(std::slice::from_ref(&sealed), &[])?;

        assert!(
            read_data_tx_metadata(&db, &tx_id).is_none(),
            "row must be deleted when {ledger:?}-block is orphaned"
        );
        Ok(())
    }

    /// Phase 1 + Phase 3 ordering: a Submit tx appears in both the orphaned
    /// block (`blocks_to_clear`) and the new canonical block
    /// (`blocks_to_confirm`).  Phase 1 must run before Phase 3 so the
    /// orphan hash is scrubbed first and the new canonical hash appended
    /// second — final state has only the new canonical hash.
    ///
    /// Guards the ordering invariant: if the phases were swapped, the
    /// new canonical hash would be appended, then the orphan hash
    /// scrubbed (no effect), leaving both hashes present.  Worse, if
    /// they referenced the same hash by accident the final state could
    /// be empty.
    #[tokio::test]
    async fn phase1_before_phase3_for_overlapping_tx() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let data_root = H256::random();
        let orphan = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let orphan_hash = orphan.block_hash;
        let new_canonical = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let new_canonical_hash = new_canonical.block_hash;
        // Distinct hashes — they're constructed with different parents so
        // even with otherwise identical contents the headers diverge.
        assert_ne!(orphan_hash, new_canonical_hash);

        // CDR currently records only the orphaned hash.
        seed_cdr(&db, data_root, tx_id, vec![orphan_hash], None);

        let orphan_sealed = make_sealed_with_submit_txs(orphan, &[(tx_id, data_root)]);
        let new_canonical_sealed =
            make_sealed_with_submit_txs(new_canonical, &[(tx_id, data_root)]);

        svc.persist_metadata(
            std::slice::from_ref(&orphan_sealed),
            std::slice::from_ref(&new_canonical_sealed),
        )?;

        let cdr = read_cdr(&db, data_root).expect("CDR present");
        assert_eq!(
            cdr.block_set,
            vec![new_canonical_hash],
            "orphan scrubbed then new canonical appended; final state has only the new hash"
        );
        assert!(
            cdr.expiry_height.is_none(),
            "expiry remains cleared post-Phase 3"
        );
        Ok(())
    }

    /// Multi-block reorg: both `blocks_to_clear` and `blocks_to_confirm` have
    /// length 2 in a single `persist_metadata` call.
    ///
    /// Guards atomicity across the loop: an early `?` exit, off-by-one, or
    /// phase ordering bug would leave one of the two txs in a stale state.
    #[tokio::test]
    async fn persist_metadata_multi_block_reorg_handles_both_slices() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        // Two distinct tx_id / data_root pairs.
        let tx_id_a = H256::random();
        let data_root_a = H256::random();
        let tx_id_b = H256::random();
        let data_root_b = H256::random();

        // Build orphan blocks (different hashes via different previous_block_hash).
        let orphan1 = make_block_header(1, H256::random(), 50, vec![tx_id_a]);
        let orphan1_hash = orphan1.block_hash;
        let orphan2 = make_block_header(2, orphan1.block_hash, 50, vec![tx_id_b]);
        let orphan2_hash = orphan2.block_hash;

        // Seed CDRs with orphan hashes in block_set and a non-None expiry_height.
        seed_cdr(&db, data_root_a, tx_id_a, vec![orphan1_hash], Some(10));
        seed_cdr(&db, data_root_b, tx_id_b, vec![orphan2_hash], Some(20));

        // Pre-write IrysDataTxMetadata rows with included_height = Some(orphan_height).
        db.update_eyre(|tx| {
            irys_database::batch_set_data_tx_included_height(tx, &[tx_id_a], 1)?;
            irys_database::batch_set_data_tx_included_height(tx, &[tx_id_b], 2)?;
            Ok(())
        })?;

        // Build canonical blocks on the new fork.
        let new1 = make_block_header(10, H256::random(), 50, vec![tx_id_a]);
        let new1_hash = new1.block_hash;
        let new2 = make_block_header(11, new1.block_hash, 50, vec![tx_id_b]);
        let new2_hash = new2.block_hash;

        let orphan1_sealed = make_sealed_with_submit_txs(orphan1, &[(tx_id_a, data_root_a)]);
        let orphan2_sealed = make_sealed_with_submit_txs(orphan2, &[(tx_id_b, data_root_b)]);
        let new1_sealed = make_sealed_with_submit_txs(new1, &[(tx_id_a, data_root_a)]);
        let new2_sealed = make_sealed_with_submit_txs(new2, &[(tx_id_b, data_root_b)]);

        // Single call with both slices of length 2.
        svc.persist_metadata(
            &[orphan1_sealed, orphan2_sealed],
            &[new1_sealed, new2_sealed],
        )?;

        // Phase 1 cleared → Phase 2 restored: included_height == new block heights.
        let meta_a = read_data_tx_metadata(&db, &tx_id_a)
            .expect("tx_a metadata must exist after re-confirmation");
        assert_eq!(
            meta_a.included_height,
            Some(10),
            "tx_a included_height must be updated to new canonical height"
        );

        let meta_b = read_data_tx_metadata(&db, &tx_id_b)
            .expect("tx_b metadata must exist after re-confirmation");
        assert_eq!(
            meta_b.included_height,
            Some(11),
            "tx_b included_height must be updated to new canonical height"
        );

        // Phase 1 scrubbed orphan hashes; Phase 3 appended new canonical hashes.
        let cdr_a = read_cdr(&db, data_root_a).expect("CDR A present");
        assert!(
            !cdr_a.block_set.contains(&orphan1_hash),
            "orphan1 hash must be scrubbed from CDR A"
        );
        assert!(
            cdr_a.block_set.contains(&new1_hash),
            "new1 hash must be appended to CDR A"
        );
        assert!(
            cdr_a.expiry_height.is_none(),
            "CDR A expiry cleared by Phase 3"
        );

        let cdr_b = read_cdr(&db, data_root_b).expect("CDR B present");
        assert!(
            !cdr_b.block_set.contains(&orphan2_hash),
            "orphan2 hash must be scrubbed from CDR B"
        );
        assert!(
            cdr_b.block_set.contains(&new2_hash),
            "new2 hash must be appended to CDR B"
        );
        assert!(
            cdr_b.expiry_height.is_none(),
            "CDR B expiry cleared by Phase 3"
        );

        Ok(())
    }
}
