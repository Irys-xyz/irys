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
    /// Epoch length, used to identify non-genesis epoch blocks: those APPLY
    /// (re-list) every commitment of the epoch but never first-INCLUDE one, so
    /// they must not write commitment dedup metadata.
    num_blocks_in_epoch: u64,
    cache: Arc<RwLock<BlockTree>>,
    chunk_migration_sender: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    storage_modules_guard: Option<StorageModulesReadGuard>,
}

/// Asserts the block's body carries exactly the tx_ids its header advertises,
/// for every data ledger (Submit / Publish / OneYear / ThirtyDay).
///
/// `transactions().get_ledger_txs(ledger)` returns `&[]` for a SealedBlock
/// whose body has been stripped, even if `header.data_ledgers[ledger].tx_ids`
/// is populated — every downstream loop that iterates body txs (block_set
/// mutation, `insert_tx_header`, etc.) silently no-ops, while
/// `write_data_ledger_metadata` reads from `header.data_ledgers` and writes
/// `included_height` / `promoted_height` anyway.  `persist_block` also
/// writes `MigratedBlockHashes[height]` body-independently.  The result is
/// `IrysDataTxMetadata` + `MigratedBlockHashes` rows for tx_ids that
/// `IrysDataTxHeaders` has no entry for — `canonical_submit_height` /
/// `canonical_promoted_height` would return `Some(height)` for those
/// tx_ids, surfacing the divergence to the validator as a consensus-
/// relevant false positive ("canonically included" for a tx_id with no
/// header).  The pre-refactor `tx_header_by_txid_canonical` masked this by
/// gating on `IrysDataTxHeaders`; the new helpers don't, so this guard is
/// now load-bearing — fail loudly here rather than let the divergence
/// escape.
///
/// Compare as sorted lists — same-length-different-ids must also fail loudly.
fn ensure_header_body_consistent(block: &SealedBlock) -> eyre::Result<()> {
    for ledger in [
        DataLedger::Submit,
        DataLedger::Publish,
        DataLedger::OneYear,
        DataLedger::ThirtyDay,
    ] {
        let mut header_ids: Vec<H256> = block
            .header()
            .data_ledgers
            .iter()
            .find(|dl| dl.ledger_id == ledger as u32)
            .map(|dl| dl.tx_ids.0.clone())
            .unwrap_or_default();
        let mut body_ids: Vec<H256> = block
            .transactions()
            .get_ledger_txs(ledger)
            .iter()
            .map(|tx| tx.id)
            .collect();
        header_ids.sort();
        body_ids.sort();
        ensure!(
            header_ids == body_ids,
            "SealedBlock {} has header/body {:?} tx mismatch; transactions appear stripped or diverged — header={:?} body={:?}",
            block.header().block_hash,
            ledger,
            header_ids,
            body_ids,
        );
    }
    Ok(())
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
        num_blocks_in_epoch: u64,
        cache: Arc<RwLock<BlockTree>>,
        chunk_migration_sender: UnboundedSender<Traced<ChunkMigrationServiceMessage>>,
    ) -> Self {
        Self {
            db,
            block_index_guard,
            supply_state,
            chunk_size,
            num_blocks_in_epoch,
            cache,
            chunk_migration_sender,
            storage_modules_guard: None,
        }
    }

    /// A non-genesis epoch block (`height % num_blocks_in_epoch == 0`, height
    /// != 0) re-lists every commitment of the epoch as an application/rollup —
    /// it never first-includes one (validators enforce exact equality with the
    /// producer's epoch rollup, so a non-genesis epoch block cannot introduce a
    /// commitment). Its confirmation/migration must therefore NOT touch
    /// commitment dedup metadata, so `included_height` keeps naming the true
    /// inclusion block and survives an epoch-boundary reorg. Genesis (height 0)
    /// is the one epoch block that genuinely first-includes its commitments, so
    /// it is excluded here and retains normal write behavior.
    fn is_non_genesis_epoch_block(&self, height: u64) -> bool {
        height != 0 && height.is_multiple_of(self.num_blocks_in_epoch)
    }

    /// Writes the commitment replay-dedup metadata (`included_height = header
    /// height`) for every commitment in `header`.
    ///
    /// Guarded by the invariant shared with [`clear_commitment_inclusions`]:
    /// only blocks that first-INCLUDE a commitment own its dedup metadata. A
    /// non-genesis epoch block merely re-lists (applies) the epoch's commitments
    /// as a rollup — writing `included_height = h_epoch` would overwrite the
    /// commitment's true inclusion height, so it is skipped. Genesis (height 0)
    /// genuinely first-includes its commitments and is NOT skipped. A
    /// commitment-free block is a no-op.
    fn persist_commitment_inclusions(
        &self,
        tx: &(impl reth_db::transaction::DbTxMut + reth_db::transaction::DbTx),
        header: &IrysBlockHeader,
    ) -> eyre::Result<()> {
        let commitment_ids = header.commitment_tx_ids();
        if commitment_ids.is_empty() || self.is_non_genesis_epoch_block(header.height) {
            return Ok(());
        }
        irys_database::batch_set_commitment_tx_included_height(tx, commitment_ids, header.height)?;
        Ok(())
    }

    /// Clears the commitment replay-dedup metadata for every commitment in
    /// `header` (re-org orphan handling). Guarded identically to
    /// [`persist_commitment_inclusions`] — see its docs for the epoch/genesis
    /// invariant: orphaning a non-genesis epoch block must not delete the dedup
    /// row of a commitment whose true (non-epoch) inclusion is still canonical.
    fn clear_commitment_inclusions(
        &self,
        tx: &(impl reth_db::transaction::DbTxMut + reth_db::transaction::DbTx),
        header: &IrysBlockHeader,
    ) -> eyre::Result<()> {
        let commitment_ids = header.commitment_tx_ids();
        if commitment_ids.is_empty() || self.is_non_genesis_epoch_block(header.height) {
            return Ok(());
        }
        irys_database::batch_clear_commitment_tx_metadata(tx, commitment_ids)?;
        Ok(())
    }

    pub fn set_storage_modules_guard(&mut self, guard: StorageModulesReadGuard) {
        self.storage_modules_guard = Some(guard);
    }

    /// Runs at block confirmation (every tip change, `block_tree_service`), NOT
    /// at migration. It does two things: (1) on reorg, clears canonical tx
    /// metadata for orphaned blocks that had already migrated; (2) maintains the
    /// non-consensus `CachedDataRoots.block_set` hint for confirmed Submit txs.
    ///
    /// It does NOT *write* canonical tx metadata (`included_height` /
    /// `promoted_height`) — that happens only at migration (`persist_block`),
    /// atomically with `MigratedBlockHashes`. Confirmation is depth 0 and its
    /// blocks aren't durably persisted until migration, so writing canonical
    /// metadata here would let an orphaned tip strand a row that a later block
    /// migrating at the same height reads as canonical.
    ///
    /// For normal blocks: `blocks_to_clear` is empty, `blocks_to_confirm` contains just the tip.
    /// For reorgs: `blocks_to_clear` contains orphaned fork blocks, `blocks_to_confirm` contains
    /// the new canonical fork blocks. Both operations happen in a single DB transaction to
    /// prevent inconsistent hint state.
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
    ///   - `persist_metadata` (this function) runs at confirmation: Phase 1
    ///     scrubs orphan block_hashes from any retained `block_set`, and
    ///     Phase 3's `update_data_root_block_set` is update-only — it never
    ///     fabricates, because by confirmation any data_root the node cares
    ///     about already has a CDR (either chunks have arrived, or the mempool
    ///     BlockConfirmed path put one there).
    ///
    /// A stale `BlockConfirmed` arriving after a reorg can re-add an orphan
    /// block_hash to `block_set`; that is tolerated because consumers treat
    /// `block_set` as a hint and re-verify canonicality through
    /// `find_canonical_ledger_range` before acting.
    ///
    /// Reorg invariant: if a Publish-promotion remains canonical, the
    /// underlying Submit inclusion must also be present in `blocks_to_confirm`
    /// — otherwise the Publish block itself would be in `blocks_to_clear`.
    /// Phase 1's clears only ever hit rows for orphaned blocks that had already
    /// *migrated* (a deep reorg past `block_migration_depth`); orphaned
    /// unmigrated blocks never wrote canonical metadata, so the clears are a
    /// no-op for them. Re-canonical blocks on the new fork get their metadata
    /// (re)written when they migrate, not here — so this function never
    /// recreates a `{None, Some}` state.
    pub fn persist_metadata(
        &self,
        blocks_to_clear: &[Arc<SealedBlock>],
        blocks_to_confirm: &[Arc<SealedBlock>],
    ) -> eyre::Result<()> {
        // Phase 1 scrub and Phase 3 append both iterate
        // `block.transactions().get_ledger_txs(Submit)`.  The in-memory
        // block-tree path always carries full transactions, but future code
        // paths (e.g. blocks hydrated from disk without their body) could
        // regress this without any error.  Surface the divergence in every
        // build so the caller fails loudly rather than committing partial
        // state.  Checked across all data ledgers because Phase 1 clears
        // orphaned metadata from `header.data_ledgers` for every ledger.
        for block in blocks_to_clear.iter().chain(blocks_to_confirm.iter()) {
            ensure_header_body_consistent(block)?;
        }

        // Defense-in-depth structural check on the reorg invariant documented
        // above: a Publish-ledger tx_id appearing in `blocks_to_confirm` while
        // its Submit row is being orphaned by `blocks_to_clear` (and NOT
        // re-confirmed in this same batch) is a malformed reorg batch — it would
        // eventually try to migrate a `promoted_height` with no prior
        // `included_height` (the illegal `{None, Some}` state), which
        // `batch_set_data_tx_promoted_height` rejects at migration. This assert
        // surfaces the caller-side construction bug earlier and more loudly in
        // debug builds, at confirmation, before the batch can reach migration.
        #[cfg(debug_assertions)]
        {
            use std::collections::HashSet;
            let confirmed_submit: HashSet<H256> = blocks_to_confirm
                .iter()
                .flat_map(|b| b.transactions().get_ledger_txs(DataLedger::Submit))
                .map(|tx| tx.id)
                .collect();
            let cleared_submit: HashSet<H256> = blocks_to_clear
                .iter()
                .flat_map(|b| b.transactions().get_ledger_txs(DataLedger::Submit))
                .map(|tx| tx.id)
                .collect();
            for block in blocks_to_confirm {
                for dl in &block.header().data_ledgers {
                    if dl.ledger_id != DataLedger::Publish as u32 {
                        continue;
                    }
                    for tx_id in &dl.tx_ids.0 {
                        debug_assert!(
                            !cleared_submit.contains(tx_id) || confirmed_submit.contains(tx_id),
                            "Publish-ledger tx {tx_id} in blocks_to_confirm has its Submit \
                             orphaned in blocks_to_clear with no re-confirmation in the same \
                             batch — caller would promote a tx whose included_height is about \
                             to be deleted",
                        );
                    }
                }
            }
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
                self.clear_commitment_inclusions(tx, header)?;

                for submit_tx in block.transactions().get_ledger_txs(DataLedger::Submit) {
                    irys_database::remove_data_root_block_set_entry(
                        tx,
                        submit_tx.data_root,
                        orphan_block_hash,
                    )?;
                }
            }
            // Phase 3: Maintain the CachedDataRoot.block_set hint for the
            //          canonical Submit-ledger txs we just confirmed (update-only,
            //          does not create cache entries for data_roots the node
            //          never tracked chunks for).
            //
            // Canonical tx metadata (`IrysDataTxMetadata` / `IrysCommitmentTxMetadata`)
            // is deliberately NOT written here. It is written only at migration
            // (`persist_block`), atomically with `MigratedBlockHashes` and the
            // block header — so a metadata row can never exist without its MBH
            // entry naming the same canonical block. Writing it here (at
            // confirmation, depth 0) previously let an orphaned tip strand a
            // metadata row that a later block migrating at the same height would
            // read as canonical. Unmigrated blocks are served branch-correctly
            // from the in-memory block tree (see `tx_inclusion` / the validator's
            // by-hash walk), so the DB metadata is only ever consulted for
            // migrated heights, where `MigratedBlockHashes` is branch-stable.
            // `block_set` is only a non-consensus hint (never read by
            // `find_canonical_ledger_range` / `canonical_*_height`), and the
            // ingress-proof pipeline needs it populated at confirmation, so it
            // stays here.
            for block in blocks_to_confirm {
                let block_hash = block.header().block_hash;
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
                    // `modules` merely overlap `range`: clip to each module's own
                    // ledger range before converting (as `index_transaction_data`
                    // does), otherwise partially covered modules are skipped and
                    // their orphaned offsets never unassigned.
                    let module_range = match module.get_storage_module_ledger_offsets() {
                        Ok(r) => r,
                        Err(err) => {
                            warn!(
                                module_id = module.id,
                                ?ledger,
                                %err,
                                "skipping storage module with no ledger offsets during rollback"
                            );
                            continue;
                        }
                    };
                    // Fully qualified method call: importing `InclusiveInterval`
                    // module-wide breaks inference on `PartitionChunkRange`,
                    // which implements the trait for two point types.
                    let Some(overlap) =
                        nodit::InclusiveInterval::intersection(&module_range, range)
                    else {
                        warn!(
                            module_id = module.id,
                            ?ledger,
                            ?range,
                            ?module_range,
                            "skipping storage module: orphaned range no longer overlaps during rollback"
                        );
                        continue;
                    };
                    // The clipped range is contained by construction, so this
                    // only fails if the module's assignment changed mid-loop.
                    let partition_range = match module.make_range_partition_relative(overlap) {
                        Ok(r) => r,
                        Err(err) => {
                            warn!(
                                module_id = module.id,
                                ?ledger,
                                %err,
                                "skipping storage module: clipped range conversion failed during rollback"
                            );
                            continue;
                        }
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
    ///
    /// Migrates the slice of blocks (oldest-first) from the index head up to `migration_block`,
    /// invoking `on_migrated` for each block the moment it is fully persisted — so a mid-batch
    /// failure still signals the blocks that did migrate.
    #[tracing::instrument(level = "trace", skip_all, fields(block.hash = %migration_block.block_hash, block.height = migration_block.height))]
    pub fn migrate_blocks(
        &self,
        migration_block: &Arc<IrysBlockHeader>,
        mut on_migrated: impl FnMut(&Arc<SealedBlock>),
    ) -> eyre::Result<()> {
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
            // Signal this block as finalized now that it is fully persisted, so a failure on a
            // later block in the batch cannot strand an already-migrated block without its frame.
            on_migrated(sealed_block);
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
        // Mirror of the `persist_metadata` guard: a stripped body would write
        // ledger metadata + block index from `header.data_ledgers` while
        // inserting zero tx-header rows from `transactions().get_ledger_txs`,
        // leaving canonical metadata pointing at txs `IrysDataTxHeaders` has
        // no entry for.  `tx_inclusion::find_canonical_ledger_range` and the
        // prior-Submit fallback both depend on those rows existing.  Checked
        // for every data ledger since `write_data_ledger_metadata` below
        // touches all of them.
        ensure_header_body_consistent(sealed_block)?;

        let header = sealed_block.header();
        let transactions = sealed_block.transactions();

        let commitment_txs = transactions.get_ledger_system_txs(SystemLedger::Commitment);
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

            // `insert_commitment_tx` above (IrysCommitments) is unconditional and
            // untouched; this only owns the replay-dedup `included_height` row.
            self.persist_commitment_inclusions(tx, header)?;

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
    //! Focused unit tests for `BlockMigrationService::persist_metadata` (the
    //! confirmation-time maintainer of the `CachedDataRoot.block_set` hint and
    //! reorg-orphan metadata cleanup) and for the migration-time metadata writer
    //! `write_data_ledger_metadata` (`IrysDataTxMetadata` /
    //! `IrysCommitmentTxMetadata`). Canonical metadata is written only at
    //! migration, never at confirmation. Also covers the
    //! `recover_from_network_partition` rollback of storage module offsets.
    //!
    //! The function only touches `self.db`; the other service fields
    //! (block_index_guard, cache, chunk_migration_sender, supply_state) are
    //! satisfied with cheap placeholders.
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, cache_data_root, open_or_create_db,
        tables::{CachedDataRoots, IrysTables},
    };
    use irys_domain::{BlockTree, StorageModuleInfo};
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::{
        BlockIndexItem, BlockTransactions, ConsensusConfig, DataTransactionHeader,
        DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        H256, H256List, IrysBlockHeader, LedgerIndexItem, NodeConfig, U256,
        partition::PartitionAssignment, partition_chunk_offset_ii,
    };
    use reth_db::Database as _;
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    /// Build a `BlockMigrationService` whose only meaningful field is `db`.
    /// All other dependencies are placeholders sufficient to construct but
    /// not exercised by `persist_metadata`. Uses the testing epoch length.
    fn make_service(
        db: DatabaseProvider,
    ) -> (
        BlockMigrationService,
        irys_testing_utils::utils::tempfile::TempDir,
    ) {
        make_service_with_epoch(db, ConsensusConfig::testing().epoch.num_blocks_in_epoch)
    }

    /// Like [`make_service`] but with an explicit `num_blocks_in_epoch`, so
    /// `persist_block` epoch-skip coverage can reach an epoch height (height 1
    /// with `num_blocks_in_epoch = 1`) after seeding only genesis, instead of
    /// seeding a full 100-block index.
    fn make_service_with_epoch(
        db: DatabaseProvider,
        num_blocks_in_epoch: u64,
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
            num_blocks_in_epoch,
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

    fn read_commitment_tx_metadata(
        db: &DatabaseProvider,
        tx_id: &H256,
    ) -> Option<irys_types::CommitmentTransactionMetadata> {
        db.view(|tx| irys_database::get_commitment_tx_metadata(tx, tx_id))
            .unwrap()
            .unwrap()
    }

    /// Build a signed header whose `SystemLedger::Commitment` ledger lists
    /// `commitment_tx_ids` (what `header.commitment_tx_ids()` reads). Data
    /// ledgers are left empty so an empty body is header/body-consistent.
    fn make_commitment_header(
        height: u64,
        previous_block_hash: H256,
        commitment_tx_ids: Vec<H256>,
    ) -> IrysBlockHeader {
        use irys_types::SystemTransactionLedger;
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = height;
        header.previous_block_hash = previous_block_hash;
        header.cumulative_diff = U256::from(height);
        header.system_ledgers = vec![SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment as u32,
            tx_ids: H256List(commitment_tx_ids),
        }];
        header.test_sign();
        header
    }

    /// Wrap a commitment header in a `SealedBlock` with an empty body. The
    /// commitment-metadata paths under test read tx_ids from the header's
    /// system ledger, not the body.
    fn make_sealed_commitment(header: IrysBlockHeader) -> Arc<SealedBlock> {
        Arc::new(SealedBlock::new_unchecked(
            Arc::new(header),
            BlockTransactions::default(),
        ))
    }

    /// Seed a commitment dedup row (`IrysCommitmentTxMetadata.included_height`)
    /// at `height`, modeling a prior inclusion.
    fn seed_commitment_included_height(db: &DatabaseProvider, tx_id: H256, height: u64) {
        db.update_eyre(|tx| {
            irys_database::set_commitment_tx_included_height(tx, &tx_id, height)?;
            Ok(())
        })
        .unwrap();
    }

    // ---- commitment included_height: epoch-block skip tests ----
    //
    // Invariant: `included_height` names the block that INCLUDED a commitment,
    // never the (non-genesis) epoch block that later APPLIED it. Epoch blocks
    // re-list every commitment of the epoch as a rollup, so their
    // confirmation/migration must not create, overwrite, or delete commitment
    // dedup rows — otherwise an epoch-boundary reorg would strand or move rows
    // whose true inclusions remain canonical below the fork point.

    /// Confirming a non-genesis epoch block must neither overwrite an existing
    /// commitment row nor create a new one.
    #[tokio::test]
    async fn persist_metadata_epoch_confirm_does_not_write_commitment_rows() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());
        let epoch_len = ConsensusConfig::testing().epoch.num_blocks_in_epoch;

        // A commitment truly included earlier at a non-epoch height.
        let included_commitment = H256::random();
        let true_inclusion_height = 5_u64;
        seed_commitment_included_height(&db, included_commitment, true_inclusion_height);

        // A commitment with no prior row (never included before this rollup).
        let fresh_commitment = H256::random();

        // Epoch block E re-lists both as a rollup at height = epoch_len.
        let epoch_block = make_commitment_header(
            epoch_len,
            H256::random(),
            vec![included_commitment, fresh_commitment],
        );
        let epoch_sealed = make_sealed_commitment(epoch_block);

        svc.persist_metadata(&[], std::slice::from_ref(&epoch_sealed))?;

        assert_eq!(
            read_commitment_tx_metadata(&db, &included_commitment)
                .expect("row must still exist")
                .included_height,
            Some(true_inclusion_height),
            "epoch-block confirm must not overwrite the true inclusion height",
        );
        assert!(
            read_commitment_tx_metadata(&db, &fresh_commitment).is_none(),
            "epoch-block confirm must not create a commitment row",
        );
        Ok(())
    }

    /// Orphaning a non-genesis epoch block (in `blocks_to_clear`) must not
    /// delete commitment rows whose true inclusion remains canonical below the
    /// fork point.
    #[tokio::test]
    async fn persist_metadata_epoch_orphan_does_not_delete_commitment_rows() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());
        let epoch_len = ConsensusConfig::testing().epoch.num_blocks_in_epoch;

        let included_commitment = H256::random();
        let true_inclusion_height = 5_u64;
        seed_commitment_included_height(&db, included_commitment, true_inclusion_height);

        let epoch_block =
            make_commitment_header(epoch_len, H256::random(), vec![included_commitment]);
        let epoch_sealed = make_sealed_commitment(epoch_block);

        // Orphan the epoch block.
        svc.persist_metadata(std::slice::from_ref(&epoch_sealed), &[])?;

        assert_eq!(
            read_commitment_tx_metadata(&db, &included_commitment)
                .expect("row must survive an epoch-block orphan")
                .included_height,
            Some(true_inclusion_height),
            "orphaning an epoch block must not delete the dedup row",
        );
        Ok(())
    }

    /// Regression guard: orphaning a NON-epoch inclusion block still clears its
    /// commitment rows (the stranded-row self-heal / last-write-wins path is
    /// preserved for inclusion blocks).
    #[tokio::test]
    async fn persist_metadata_non_epoch_orphan_still_clears_commitment_rows() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let commitment = H256::random();
        let height = 5_u64; // non-epoch (5 % 100 != 0)
        seed_commitment_included_height(&db, commitment, height);

        let block = make_commitment_header(height, H256::random(), vec![commitment]);
        let sealed = make_sealed_commitment(block);

        svc.persist_metadata(std::slice::from_ref(&sealed), &[])?;

        assert!(
            read_commitment_tx_metadata(&db, &commitment).is_none(),
            "orphaning a non-epoch inclusion block must still clear its commitment row",
        );
        Ok(())
    }

    /// Genesis (height 0) is excluded from the epoch skip: it genuinely
    /// first-includes its commitments, so orphaning it clears the dedup row like
    /// any inclusion block. Metadata is written at migration (`persist_block`),
    /// so we seed the migrated row, then orphan. (The genesis migration write is
    /// covered by `persist_block_epoch_block_skips_commitment_included_height`.)
    #[tokio::test]
    async fn persist_metadata_genesis_orphan_clears_commitment_rows() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let commitment = H256::random();
        let genesis = make_commitment_header(0, H256::zero(), vec![commitment]);
        let genesis_sealed = make_sealed_commitment(genesis);

        // Migrated state: genesis wrote its commitment dedup row at migration.
        seed_commitment_included_height(&db, commitment, 0);

        svc.persist_metadata(std::slice::from_ref(&genesis_sealed), &[])?;
        assert!(
            read_commitment_tx_metadata(&db, &commitment).is_none(),
            "genesis orphan clears the commitment dedup row (height 0 not epoch-skipped)",
        );
        Ok(())
    }

    /// Migration (`persist_block`) of a non-genesis epoch block must not write
    /// commitment `included_height`, while genesis migration still does.
    /// Uses `num_blocks_in_epoch = 1` so height 1 is an epoch block reachable
    /// after seeding only genesis (avoids seeding a full 100-block index).
    #[tokio::test]
    async fn persist_block_epoch_block_skips_commitment_included_height() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service_with_epoch(db.clone(), 1);

        // Genesis (height 0) is not skipped: migrating it writes the row.
        let genesis_commitment = H256::random();
        let genesis = make_commitment_header(0, H256::zero(), vec![genesis_commitment]);
        let genesis_sealed = make_sealed_commitment(genesis);
        svc.persist_block(genesis_sealed.as_ref())?;
        assert_eq!(
            read_commitment_tx_metadata(&db, &genesis_commitment).and_then(|m| m.included_height),
            Some(0),
            "genesis migration writes commitment included_height",
        );

        // Epoch block at height 1 (num_blocks_in_epoch = 1): migrating it must
        // NOT write the commitment row.
        let epoch_commitment = H256::random();
        let epoch_block = make_commitment_header(
            1,
            genesis_sealed.header().block_hash,
            vec![epoch_commitment],
        );
        let epoch_sealed = make_sealed_commitment(epoch_block);
        svc.persist_block(epoch_sealed.as_ref())?;
        assert!(
            read_commitment_tx_metadata(&db, &epoch_commitment).is_none(),
            "migration of a non-genesis epoch block must not write commitment included_height",
        );
        Ok(())
    }

    // ---- included_height / promoted_height correctness tests ----

    /// Submit→Publish promotion: after confirming in a Submit block at height H,
    /// then a Publish block at height H+N, `included_height` must still be H
    /// and `promoted_height` must be H+N.
    #[tokio::test]
    async fn submit_to_publish_promotion_preserves_included_height() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (_svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();
        let submit_height = 1_u64;
        let publish_height = 5_u64;

        // Migrate the Submit-ledger block at height 1 (metadata is written at
        // migration via `write_data_ledger_metadata`, never at confirmation).
        let submit_header =
            make_block_header_with_ledger(submit_height, H256::random(), DataLedger::Submit, tx_id);
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &submit_header.data_ledgers, submit_height)?;
            Ok(())
        })?;

        // Migrate the Publish-ledger block promoting the same tx at height 5
        let publish_header = make_block_header_with_ledger(
            publish_height,
            H256::random(),
            DataLedger::Publish,
            tx_id,
        );
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &publish_header.data_ledgers, publish_height)?;
            Ok(())
        })?;

        let meta = read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after migration");
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

    /// Same-block Submit→Publish promotion at migration: the term-ledger
    /// metadata write must happen before the Publish write so `promoted_height`
    /// can be set in the same transaction. (Covered via `persist_block` — the
    /// real migration path — below.)
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
        let (_svc, _svc_tmp) = make_service(db.clone());

        let tx_id = H256::random();

        let header = make_block_header_with_ledger(height, H256::random(), ledger, tx_id);
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &header.data_ledgers, height)?;
            Ok(())
        })?;

        let meta = read_data_tx_metadata(&db, &tx_id).expect("metadata must exist after migration");
        assert_eq!(meta.included_height, Some(height));
        assert_eq!(meta.promoted_height, None);
        Ok(())
    }

    /// Root-fix guard: `persist_metadata` (confirmation, depth 0) writes NO
    /// canonical tx metadata — `IrysDataTxMetadata` / `IrysCommitmentTxMetadata`
    /// are written only at migration (`persist_block`). It still maintains the
    /// non-consensus `block_set` hint. Writing canonical metadata at confirmation
    /// would let an orphaned tip strand a row that a later block migrating at the
    /// same height reads as canonical.
    #[tokio::test]
    async fn confirm_does_not_write_canonical_metadata() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        // A Submit-ledger data tx with a pre-existing CDR (Phase 3 is update-only).
        let tx_id = H256::random();
        let data_root = H256::random();
        seed_cdr(&db, data_root, tx_id, vec![], None);
        let header = make_block_header(1, H256::random(), 50, vec![tx_id]);
        let block_hash = header.block_hash;
        let sealed = make_sealed_with_submit_txs(header, &[(tx_id, data_root)]);

        // A commitment tx in a separate confirmed block.
        let commitment = H256::random();
        let commitment_sealed =
            make_sealed_commitment(make_commitment_header(2, H256::random(), vec![commitment]));

        svc.persist_metadata(&[], &[sealed, commitment_sealed])?;

        // No canonical metadata written at confirmation.
        assert!(
            read_data_tx_metadata(&db, &tx_id).is_none(),
            "confirmation must NOT write IrysDataTxMetadata (written at migration only)"
        );
        assert!(
            read_commitment_tx_metadata(&db, &commitment).is_none(),
            "confirmation must NOT write IrysCommitmentTxMetadata (written at migration only)"
        );

        // But the non-consensus block_set hint IS maintained (Phase 3).
        let cdr = read_cdr(&db, data_root).expect("CDR present");
        assert!(
            cdr.block_set.contains(&block_hash),
            "confirmation still maintains the block_set hint the ingress pipeline needs"
        );
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

        // Migrate the Submit block at height 2 (metadata written at migration)
        let submit_header =
            make_block_header_with_ledger(submit_height, H256::random(), DataLedger::Submit, tx_id);
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &submit_header.data_ledgers, submit_height)?;
            Ok(())
        })?;

        // Migrate the Publish block promoting the tx at height 8
        let publish_header = make_block_header_with_ledger(
            publish_height,
            H256::random(),
            DataLedger::Publish,
            tx_id,
        );
        let publish_sealed =
            make_sealed_single_ledger_tx(publish_header, DataLedger::Publish, tx_id);
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &publish_sealed.header().data_ledgers, publish_height)?;
            Ok(())
        })?;

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
        // Migrated state: the Submit block wrote its metadata at migration.
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &sealed.header().data_ledgers, height)?;
            Ok(())
        })?;

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
        // Migrated state: the term-ledger block wrote its metadata at migration.
        db.update_eyre(|tx| {
            write_data_ledger_metadata(tx, &sealed.header().data_ledgers, height)?;
            Ok(())
        })?;

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

        // Phase 1 cleared the orphaned rows. Confirmation no longer re-writes
        // canonical metadata — the new-fork blocks write their own
        // `included_height` when they migrate — so both rows are absent here.
        assert!(
            read_data_tx_metadata(&db, &tx_id_a).is_none(),
            "tx_a metadata cleared on reorg; re-written only when the new fork migrates"
        );
        assert!(
            read_data_tx_metadata(&db, &tx_id_b).is_none(),
            "tx_b metadata cleared on reorg; re-written only when the new fork migrates"
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

    /// Multi-block reorg where one cleared tx is permanently orphaned —
    /// present in `blocks_to_clear` but NOT re-confirmed in
    /// `blocks_to_confirm`.  Companion to
    /// `persist_metadata_multi_block_reorg_handles_both_slices`, which only
    /// asserts the re-confirmation case.
    ///
    /// Expected post-state for the permanently-orphaned tx:
    ///   - Metadata row fully deleted (Phase 1 term-ledger unconditional
    ///     delete; no Phase 2 recreation).
    ///   - CDR has the orphan block hash scrubbed (Phase 1) but no new hash
    ///     appended (Phase 3 only touches CDRs whose tx is in
    ///     `blocks_to_confirm`).
    ///   - CDR `expiry_height` is left untouched — the mempool reorg
    ///     re-anchor path (`handle_confirmed_data_tx_reorg`) is responsible
    ///     for refreshing it once the orphaned tx is re-ingested.
    #[tokio::test]
    async fn persist_metadata_multi_block_reorg_permanent_orphan() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db.clone());

        let tx_id_a = H256::random();
        let data_root_a = H256::random();
        let tx_id_b = H256::random();
        let data_root_b = H256::random();

        // Orphan fork: orphan1 carries tx_a, orphan2 carries tx_b.
        let orphan1 = make_block_header(1, H256::random(), 50, vec![tx_id_a]);
        let orphan1_hash = orphan1.block_hash;
        let orphan2 = make_block_header(2, orphan1.block_hash, 50, vec![tx_id_b]);
        let orphan2_hash = orphan2.block_hash;

        // Seed CDRs and pre-write included_height as if both txs were once
        // confirmed on the orphan fork.
        seed_cdr(&db, data_root_a, tx_id_a, vec![orphan1_hash], Some(10));
        seed_cdr(&db, data_root_b, tx_id_b, vec![orphan2_hash], Some(20));
        db.update_eyre(|tx| {
            irys_database::batch_set_data_tx_included_height(tx, &[tx_id_a], 1)?;
            irys_database::batch_set_data_tx_included_height(tx, &[tx_id_b], 2)?;
            Ok(())
        })?;

        // New canonical fork re-confirms ONLY tx_a.  tx_b is permanently orphaned.
        let new1 = make_block_header(10, H256::random(), 50, vec![tx_id_a]);
        let new1_hash = new1.block_hash;

        let orphan1_sealed = make_sealed_with_submit_txs(orphan1, &[(tx_id_a, data_root_a)]);
        let orphan2_sealed = make_sealed_with_submit_txs(orphan2, &[(tx_id_b, data_root_b)]);
        let new1_sealed = make_sealed_with_submit_txs(new1, &[(tx_id_a, data_root_a)]);

        svc.persist_metadata(&[orphan1_sealed, orphan2_sealed], &[new1_sealed])?;

        // tx_a: cleared by Phase 1. Confirmation no longer re-writes metadata,
        // so it's absent until the new fork migrates — the re-confirmation is
        // visible only in the CDR block_set below.
        assert!(
            read_data_tx_metadata(&db, &tx_id_a).is_none(),
            "tx_a metadata cleared on reorg; re-written only when the new fork migrates"
        );

        // tx_b: row deleted by Phase 1 (permanently orphaned, never re-confirmed).
        assert!(
            read_data_tx_metadata(&db, &tx_id_b).is_none(),
            "permanently-orphaned tx_b metadata row must be deleted"
        );

        // CDR A: re-confirmation path — orphan scrubbed, new canonical hash
        // appended, expiry cleared by Phase 3.
        let cdr_a = read_cdr(&db, data_root_a).expect("CDR A present");
        assert!(!cdr_a.block_set.contains(&orphan1_hash));
        assert!(cdr_a.block_set.contains(&new1_hash));
        assert!(cdr_a.expiry_height.is_none());

        // CDR B: permanent-orphan path — orphan hash scrubbed, no new hash
        // appended (Phase 3 doesn't run for un-re-confirmed txs).  Block_set
        // is now empty, but expiry_height stays at the pre-reorg value
        // because Phase 3 never executed for this CDR.
        let cdr_b = read_cdr(&db, data_root_b).expect("CDR B present");
        assert!(
            !cdr_b.block_set.contains(&orphan2_hash),
            "orphan2 hash must be scrubbed from CDR B"
        );
        assert!(
            cdr_b.block_set.is_empty(),
            "CDR B block_set must be empty after permanent-orphan scrub \
             (no Phase 3 append for un-re-confirmed tx)"
        );
        assert_eq!(
            cdr_b.expiry_height,
            Some(20),
            "CDR B expiry_height must remain untouched — Phase 3 doesn't run \
             when the tx is absent from `blocks_to_confirm`"
        );

        Ok(())
    }

    /// Regression for the broadened `ensure_header_body_consistent` check:
    /// every data ledger (Submit / Publish / OneYear / ThirtyDay) must
    /// trigger the guard when its header advertises a tx_id its body lacks.
    /// Prior to broadening, only Submit was checked, so a stripped
    /// OneYear/ThirtyDay/Publish body would silently write `included_height` /
    /// `promoted_height` rows alongside `MigratedBlockHashes[height]` for
    /// tx_ids `IrysDataTxHeaders` has no entry for — `canonical_submit_height`
    /// / `canonical_promoted_height` would return `Some(height)` for them
    /// (both metadata and MBH get written body-independently), surfacing the
    /// divergence to the validator as a consensus-relevant false positive.
    #[rstest::rstest]
    #[case::submit(DataLedger::Submit)]
    #[case::publish(DataLedger::Publish)]
    #[case::oneyear(DataLedger::OneYear)]
    #[case::thirtyday(DataLedger::ThirtyDay)]
    #[tokio::test]
    async fn persist_metadata_rejects_stripped_body_for_any_ledger(
        #[case] ledger: DataLedger,
    ) -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (svc, _svc_tmp) = make_service(db);

        let tx_id = H256::random();
        let header = make_block_header_with_ledger(1, H256::random(), ledger, tx_id);
        // Stripped body: header advertises `tx_id` in `ledger`, but the
        // BlockTransactions map has no entry for that ledger at all.
        let stripped = Arc::new(SealedBlock::new_unchecked(
            Arc::new(header),
            BlockTransactions::default(),
        ));

        let err = svc
            .persist_metadata(&[], std::slice::from_ref(&stripped))
            .expect_err("stripped body must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("header/body") && msg.contains(&format!("{ledger:?}")),
            "error must call out the offending ledger; got: {msg}"
        );

        // And the same for `persist_block`, which mirrors the guard.
        let err = svc
            .persist_block(&stripped)
            .expect_err("persist_block stripped body must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("header/body") && msg.contains(&format!("{ledger:?}")),
            "persist_block error must call out the offending ledger; got: {msg}"
        );

        Ok(())
    }

    /// Regression test: an orphaned chunk range spanning a storage module
    /// (slot) boundary only partially overlaps each module, so
    /// `recover_from_network_partition` must clip the range to each module
    /// before converting it to partition offsets. The unclipped conversion
    /// underflowed for the higher module and overshot the lower one.
    #[test]
    fn recover_from_network_partition_handles_ranges_spanning_modules() -> eyre::Result<()> {
        let (db, _db_tmp) = open_db()?;
        let (mut svc, _svc_tmp) = make_service(db.clone());

        // Two Submit-ledger modules of 5 chunks each: slot 0 covers ledger
        // offsets [0, 4], slot 1 covers [5, 9].
        let sm_tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("recover_spanning_modules")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: sm_tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let mut modules = Vec::new();
        for slot in 0..2_usize {
            let info = StorageModuleInfo {
                id: slot,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit as u32),
                    slot_index: Some(slot),
                    ..PartitionAssignment::default()
                }),
                submodules: vec![(
                    partition_chunk_offset_ii!(0, 4),
                    format!("submodule-{slot}").into(),
                )],
            };
            let module = Arc::new(StorageModule::new(&info, &config)?);
            module.pack_with_zeros();
            modules.push(module);
        }
        svc.set_storage_modules_guard(StorageModulesReadGuard::new(Arc::new(RwLock::new(
            modules.clone(),
        ))));

        // Genesis leaves the Submit ledger at 3 chunks; the orphaned block
        // takes it to 8, so its range [3, 7] crosses the slot boundary at 5.
        let genesis = make_block_header(0, H256::zero(), 3, vec![]);
        let orphan = make_block_header(1, genesis.block_hash, 8, vec![]);
        db.update_eyre(|tx| {
            irys_database::insert_block_header(tx, &genesis)?;
            irys_database::insert_block_header(tx, &orphan)
        })?;
        let submit_index_item = |header: &IrysBlockHeader, total_chunks| BlockIndexItem {
            block_hash: header.block_hash,
            num_ledgers: 1,
            ledgers: vec![LedgerIndexItem {
                total_chunks,
                tx_root: H256::zero(),
                ledger: DataLedger::Submit,
            }],
        };
        {
            let index = svc.block_index_guard.read();
            index.push_item(&submit_index_item(&genesis, 3), 0)?;
            index.push_item(&submit_index_item(&orphan, 8), 1)?;
        }

        svc.recover_from_network_partition(0)?;

        {
            let index = svc.block_index_guard.read();
            assert_eq!(index.num_blocks(), 1);
            assert_eq!(index.latest_height(), 0);
        }

        // Each module had exactly its own slice of [3, 7] unassigned: the
        // genesis chunks [0, 2] stay packed in slot 0, and slot 1's offsets
        // past the orphaned range (ledger [8, 9]) stay packed.
        assert_eq!(
            modules[0].get_intervals(ChunkType::Uninitialized),
            [partition_chunk_offset_ii!(3, 4)]
        );
        assert_eq!(
            modules[0].get_intervals(ChunkType::Entropy),
            [partition_chunk_offset_ii!(0, 2)]
        );
        assert_eq!(
            modules[1].get_intervals(ChunkType::Uninitialized),
            [partition_chunk_offset_ii!(0, 2)]
        );
        assert_eq!(
            modules[1].get_intervals(ChunkType::Entropy),
            [partition_chunk_offset_ii!(3, 4)]
        );

        Ok(())
    }
}
