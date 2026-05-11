use crate::chunk_ingress_service::ChunkIngressServiceInner;
use crate::chunk_ingress_service::ingress_proofs::{
    IngressProofGenerationState, RegenAction, generate_and_store_ingress_proof,
    reanchor_and_store_ingress_proof,
};
use crate::metrics;
use irys_database::{
    cached_data_root_by_data_root, delete_ingress_proof_if_unchanged,
    delete_unprotected_cached_chunks_older_than, get_data_tx_metadata,
    reclaim_cached_chunks_older_than, tx_header_by_txid,
};
use irys_database::{
    db::{DatabaseProviderCacheExt as _, IrysDatabaseExt as _},
    delete_cached_chunks_by_data_root, get_cache_size,
    scoped_tx::{Cache, ScopedTx},
    tables::{
        CachedChunks, CachedChunksIndex, CachedDataRoots, CompactCachedIngressProof, IngressProofs,
    },
};
use irys_domain::{
    BlockBoundsError, BlockIndexReadGuard, BlockTreeReadGuard, ChainState, EpochSnapshot,
    StorageModulesReadGuard,
};
use irys_types::ingress::CachedIngressProof;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    Config, DataLedger, DataRoot, DataTransactionHeader, DatabaseProvider, GIGABYTE, H256,
    IngressProof, IrysBlockHeader, LedgerChunkOffset, PartitionChunkOffset, SendTraced as _,
    TokioServiceHandle, Traced, TxChunkOffset, UnixTimestamp,
};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Condvar;
#[cfg(test)]
use std::sync::Mutex;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{Instrument as _, debug, error, info, trace, warn};

/// Outcome of the CDR pruning-horizon policy for one `CachedDataRoots` entry.
struct CdrEvictionState {
    inclusion_max_height: Option<u64>,
    horizon: Option<u64>,
    evictable: bool,
}

/// One transaction's chunk span on a ledger, in ledger offsets.
struct TxLedgerSpan {
    ledger: DataLedger,
    /// First chunk of this transaction on `ledger`.
    start: LedgerChunkOffset,
    /// Chunk length. `start + chunk_count` may not fit in a ledger offset.
    chunk_count: u64,
}

/// `start + count`. `None` when the sum does not fit in a ledger offset.
fn checked_ledger_offset(start: LedgerChunkOffset, count: u64) -> Option<LedgerChunkOffset> {
    (*start).checked_add(count).map(LedgerChunkOffset::from)
}

/// Maximum evictions per invocation to prevent blocking the service.
/// If this limit is reached, eviction will continue in the next cycle.
const MAX_EVICTIONS_PER_RUN: usize = 10_000;

/// Whether every tx of a cached data root is migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootMigration {
    Migrated,
    /// No `CachedDataRoots` row for the root.
    NoRecord,
    /// The row lists no txs.
    NoTxids,
    /// A tx has no inclusion height, or one above the migration height.
    TxPending,
}

/// Why one chunk-pruning pass kept or deleted each cached body it looked at.
/// `too_young` covers every body the scan visited; the rest cover old bodies
/// collected as candidates, each counted once.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PruneTally {
    /// Old enough to evict, collected this pass.
    pub candidates: u64,
    /// Deleted: persisted `Data` here with this signer's ingress hash.
    pub deleted_durable: u64,
    /// Deleted: not stored or proven here, and its root is migrated.
    pub deleted_unplaced: u64,
    /// Kept: younger than migration depth plus the cache clean lag.
    pub too_young: u64,
    /// Kept: placed, assigned, or proven here, and the offset is not
    /// persisted `Data` yet.
    pub not_durable: u64,
    /// Kept: persisted `Data` here, but this signer has no compact ingress
    /// hash for the chunk. The durable path never deletes these.
    pub no_ingress_hash: u64,
    /// Kept: not durable here, and some tx of its root is not migrated yet.
    pub root_not_migrated: u64,
    /// Of `root_not_migrated`: the root has no `CachedDataRoots` row, so it
    /// can never count as migrated.
    pub root_no_record: u64,
    /// Of `root_not_migrated`: the root's row lists no txs.
    pub root_no_txids: u64,
    /// Of `root_not_migrated`: a local storage module has a placement for
    /// the chunk. The rest are placed nowhere on this node.
    pub root_not_migrated_placed: u64,
    /// Kept: storage metadata for the root could not be read.
    pub unreadable: u64,
    /// Offered to the unplaced path. What it did not delete was placed or
    /// updated since the scan.
    pub unplaced_attempted: u64,
}

impl PruneTally {
    pub(crate) fn unplaced_kept(&self) -> u64 {
        self.unplaced_attempted
            .saturating_sub(self.deleted_unplaced)
    }
}
/// Maximum ingress proofs to scan per pruning invocation.
const MAX_PROOF_CHECKS_PER_RUN: usize = 1_000;

#[derive(Debug)]
pub enum CacheServiceAction {
    OnBlockMigrated(u64, Option<oneshot::Sender<eyre::Result<()>>>),
    OnEpochProcessed(
        Arc<EpochSnapshot>,
        Option<oneshot::Sender<eyre::Result<()>>>,
    ),
    /// Internal signal: pruning task completed
    PruneCompleted(eyre::Result<()>),
    /// Internal signal: epoch processing task completed
    EpochProcessingCompleted(eyre::Result<()>),
    /// Internal signal: txid pruning task completed
    PruneTxidsCompleted(eyre::Result<()>),
    /// Remove specific txids from `CachedDataRoot.txid_set` entries.
    /// Sent by the mempool when txs are pruned (anchor expired) to prevent stale
    /// txid references from blocking publish candidate selection.
    PruneTxidsFromCachedDataRoots(HashMap<H256, Vec<H256>>),
}

/// One pause between the durability snapshot and the delete re-check.
///
/// A test marks an offset `Uninitialized` in that gap. The re-check must
/// then keep the cached body.
#[cfg(test)]
#[derive(Debug)]
struct PruneRecheckPause {
    armed: Mutex<bool>,
    arrived: Mutex<bool>,
    arrived_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

#[cfg(test)]
impl PruneRecheckPause {
    fn new() -> Self {
        Self {
            armed: Mutex::new(false),
            arrived: Mutex::new(false),
            arrived_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }
}

/// Context for cache pruning tasks. You can safely clone this struct to spawn pruning tasks on
/// separate runtimes/threads.
#[derive(Debug, Clone)]
pub struct InnerCacheTask {
    pub db: DatabaseProvider,
    pub block_tree_guard: BlockTreeReadGuard,
    pub block_index_guard: BlockIndexReadGuard,
    pub config: Config,
    pub gossip_broadcast: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    pub ingress_proof_generation_state: IngressProofGenerationState,
    pub cache_sender: CacheServiceSender,
    /// Read access to local storage modules, used to answer "is this chunk
    /// body durably stored?" at prune time.
    pub storage_modules_guard: StorageModulesReadGuard,
    /// Shared by a cloned task so a test thread can release the prune thread.
    #[cfg(test)]
    prune_recheck_pause: Arc<PruneRecheckPause>,
}

/// Outcome of one [`InnerCacheTask::try_prune_txids_once`] attempt.
#[derive(Debug, PartialEq, Eq)]
enum TxidScrubAttempt {
    /// The scrub ran to completion.
    Done,
    /// The block-tree lock was write-held; nothing was written.
    Deferred,
}

impl InnerCacheTask {
    /// Processes epoch completion by pruning expired data roots.
    ///
    /// Determines the pruning horizon (one block before active submit ledger slots begin)
    /// and removes data roots from earlier blocks that are no longer needed for validation.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - First unexpired slot index overflows u64
    /// - Block index or canonical chain lookup fails
    /// - Database pruning operation fails
    fn on_epoch_processed(&self, epoch_snapshot: Arc<EpochSnapshot>) -> eyre::Result<()> {
        let ledger_id = DataLedger::Submit;
        let raw_index = epoch_snapshot.get_first_unexpired_slot_index(ledger_id);
        let first_unexpired_slot_index: u64 = raw_index.try_into().map_err(|e| {
            eyre::eyre!(
                "first_unexpired_slot_index overflow: value {} exceeds u64::MAX: {}",
                raw_index,
                e
            )
        })?;

        let chunk_offset =
            first_unexpired_slot_index * self.config.consensus.num_chunks_in_partition;

        // Check to see if the first overlapping block in our first active submit ledger slot is in the block index
        let mut prune_height: Option<u64> = None;
        match self
            .block_index_guard
            .read()
            .get_block_bounds(ledger_id, LedgerChunkOffset::from(chunk_offset))
        {
            Ok(block_bounds) => {
                // Height 0 has no submit-ledger rows in the block index.
                prune_height = Some(block_bounds.height.saturating_sub(1));
            }
            // Nothing indexed at this offset (yet) — fall through to the
            // canonical-chain scan below.
            Err(
                BlockBoundsError::IndexEmpty
                | BlockBoundsError::LedgerInactive { .. }
                | BlockBoundsError::OffsetBeyondFrontier { .. },
            ) => {}
            Err(BlockBoundsError::Internal(e)) => {
                return Err(e.wrap_err("Failed to get block bounds for cache pruning"));
            }
        }

        if prune_height.is_none() {
            let (canonical, _) = self.block_tree_guard.read().get_canonical_chain();

            let found_block = canonical.iter().rev().find_map(|block_entry| {
                let block = block_entry.header();
                let ledger_total_chunks = block.data_ledgers[ledger_id].total_chunks;
                if ledger_total_chunks <= chunk_offset {
                    Some((block_entry.height(), ledger_total_chunks))
                } else {
                    None
                }
            });
            let (block_height, _ledger_max_offset) = found_block.ok_or_else(|| {
                eyre::eyre!(
                    "No block found in canonical chain with ledger_total_chunks <= {}. Chain may be incomplete or corrupted.",
                    chunk_offset
                )
            })?;
            // Height 0 has no submit-ledger rows on the canonical chain.
            prune_height = Some(block_height.saturating_sub(1));
        }

        let prune_height = prune_height.ok_or_else(|| {
            eyre::eyre!(
                "Unable to determine prune height. First unexpired slot: {}",
                first_unexpired_slot_index
            )
        })?;
        self.prune_data_root_cache(prune_height)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(migration_height))]
    fn prune_cache(&self, migration_height: u64) -> eyre::Result<()> {
        // First, prune ingress proofs to figure out what data roots are still in use
        self.prune_ingress_proofs()?;

        let prune_height = migration_height
            .saturating_sub(u64::from(self.config.node_config.cache.cache_clean_lag));
        debug!(
            "Pruning cache for height {} ({})",
            &migration_height, &prune_height
        );

        let ((chunk_cache_count, chunk_cache_size), ingress_proof_count) =
            self.db.view_cache_eyre(|tx| {
                Ok((
                    get_cache_size::<CachedChunks, _>(
                        tx.inner(),
                        self.config.consensus.chunk_size,
                    )?,
                    tx.entries::<IngressProofs>()?,
                ))
            })?;
        info!(
            custom.migration_height= ?migration_height,
            "Chunk cache: {} chunks ({:.3} GB),  {} ingress proofs",
            chunk_cache_count,
            (chunk_cache_size / GIGABYTE as u64),
            ingress_proof_count
        );
        metrics::record_cache_stats(chunk_cache_count, chunk_cache_size);

        // Attempt pruning cache only if we're above `prune_at_capacity_percent`% of max capacity.
        let max_cache_size_bytes = self.config.node_config.cache.max_cache_size_bytes;
        let threshold_bytes = max_cache_size_bytes as f64
            * (self.config.node_config.cache.prune_at_capacity_percent / 100_f64);
        if chunk_cache_size as f64 > threshold_bytes {
            debug!("Cache above target capacity, proceeding with pruning");
            if chunk_cache_size > max_cache_size_bytes {
                warn!(
                    "Cache above configured soft maximum; bodies this node stores or proves stay until durable: size: {} max: {}",
                    &chunk_cache_size, &max_cache_size_bytes
                )
            }
            // This is a correctness-constrained soft target: bodies that have
            // not completed the durable write/leaf handoff may temporarily
            // keep the cache above its configured maximum. Normal terminal
            // lifecycle pruning still removes expired or abandoned roots.
            self.prune_durably_migrated_chunk_bodies(migration_height)?;
        } else {
            debug!("Cache under target capacity, skipping chunk pruning");
        }

        Ok(())
    }

    /// True when this node's own ingress proof for `data_root` is present.
    /// Shared by the scan and write phases of both prune passes so the
    /// local-proof exemption cannot diverge between collect-time and
    /// delete-time (the RO scan and the gated write are separate txns).
    fn has_local_ingress_proof<T: reth_db::transaction::DbTx>(
        tx: &T,
        data_root: DataRoot,
        local_address: irys_types::IrysAddress,
    ) -> eyre::Result<bool> {
        let mut proofs_cursor = tx.cursor_read::<IngressProofs>()?;
        let mut walker = proofs_cursor.walk(Some(data_root))?;
        while let Some((key, compact)) = walker.next().transpose()? {
            if key != data_root {
                break;
            }
            if compact.0.address == local_address {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the root has at least one tx and every one is included at or
    /// below `migration_height`, and if not, why. Placements exist only after
    /// migration.
    fn root_migration<T: reth_db::transaction::DbTx>(
        tx: &ScopedTx<Cache>,
        consensus_tx: &T,
        data_root: DataRoot,
        migration_height: u64,
    ) -> eyre::Result<RootMigration> {
        let Some(cached) = tx.get::<CachedDataRoots>(data_root)? else {
            return Ok(RootMigration::NoRecord);
        };
        if cached.txid_set.is_empty() {
            return Ok(RootMigration::NoTxids);
        }
        for tx_id in cached.txid_set.iter() {
            match get_data_tx_metadata(consensus_tx, tx_id)?.and_then(|m| m.included_height) {
                Some(height) if height <= migration_height => {}
                _ => return Ok(RootMigration::TxPending),
            }
        }
        Ok(RootMigration::Migrated)
    }

    /// Evaluates the CDR pruning-horizon policy for `cached`, reading tx
    /// metadata through `tx`. Shared by `prune_data_root_cache`'s scan and
    /// write phases so the delete-time re-check applies exactly the
    /// collect-time policy.
    fn cdr_eviction_state<T: reth_db::transaction::DbTx>(
        tx: &T,
        cached: &irys_database::db_cache::CachedDataRoot,
        prune_height: u64,
    ) -> eyre::Result<CdrEvictionState> {
        let mut inclusion_max_height: Option<u64> = None;
        let mut all_txs_confirmed = true;
        for tx_id in cached.txid_set.iter() {
            match get_data_tx_metadata(tx, tx_id)?.and_then(|m| m.included_height) {
                Some(h) => {
                    inclusion_max_height = Some(inclusion_max_height.map_or(h, |x| x.max(h)));
                }
                None => {
                    all_txs_confirmed = false;
                    break;
                }
            }
        }
        let horizon = if all_txs_confirmed {
            match (inclusion_max_height, cached.expiry_height) {
                (Some(h), _) => Some(h),
                (None, Some(e)) => Some(e),
                (None, None) => None,
            }
        } else {
            cached.expiry_height
        };
        let evictable = match (horizon, all_txs_confirmed) {
            (Some(max_height), _) => max_height < prune_height,
            (None, true) => true,
            (None, false) => false,
        };
        Ok(CdrEvictionState {
            inclusion_max_height,
            horizon,
            evictable,
        })
    }

    /// Capacity-prunes cached bodies older than the migration age.
    ///
    /// A body stays when this node holds a local ingress proof, a placement
    /// row, or a module assignment that has not persisted this chunk.
    /// Open drops the placement row when the file is not `Data`. The assignment
    /// remains, and the cached body is what a replica reads back until the
    /// file says `Data` for this chunk. A `Data` label whose index names
    /// another chunk is that chunk's bytes.
    ///
    /// A placed or proven body is deleted only by
    /// `delete_reclaimable_cached_chunks_older_than`: the offset must be
    /// persisted `Data`, and the current signer's compact ingress hash must
    /// be present. A body with neither a proof, a placement, nor an
    /// assignment that still owes this chunk is deleted on age once every tx
    /// of its root is migrated. That path does not require the ingress hash.
    /// Before migration
    /// no node has a placement yet, so "unassigned" cannot be told apart from
    /// "assigned, not migrated".
    #[tracing::instrument(level = "trace", skip_all)]
    fn prune_durably_migrated_chunk_bodies(
        &self,
        migration_height: u64,
    ) -> eyre::Result<PruneTally> {
        let local_address = self.config.irys_signer().address();
        let min_chunk_age_in_blocks = self
            .config
            .consensus
            .block_migration_depth
            .saturating_add(self.config.node_config.cache.cache_clean_lag as u32);
        let target_time_between_blocks_secs =
            self.config.consensus.difficulty_adjustment.block_time;
        let min_chunk_age_secs =
            u64::from(min_chunk_age_in_blocks).saturating_mul(target_time_between_blocks_secs);
        let now = UnixTimestamp::now()
            .expect("unable to get current unix timestamp")
            .as_secs();
        let delete_chunks_older_than =
            UnixTimestamp::from_secs(now.saturating_sub(min_chunk_age_secs));

        // Scan candidates under a short-lived read txn, then drop it before any
        // gated write (same shape as `prune_ingress_proofs`): holding a RO txn
        // across cache-writer-gate waits pins MVCC free-page reclamation.
        let mut candidates: BTreeMap<DataRoot, BTreeSet<TxChunkOffset>> = BTreeMap::new();
        let mut tally = PruneTally::default();
        {
            let tx = ScopedTx::<Cache>::new(self.db.cache().tx()?);
            let mut chunk_cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
            let seek_key: DataRoot = H256::random();
            let mut chunk_walk = chunk_cursor.walk(Some(seek_key))?;
            let mut wrapped = false;

            // Cap is on roots with old cached bodies collected in the read
            // phase, not on roots that ultimately yield a durable deletion.
            // The scan therefore starts at a
            // random key and wraps (same shape as `prune_data_root_cache`), so
            // a stable set of non-yielding candidates early in hash order
            // cannot consume the budget every run and starve later prunable
            // roots. The wrap stops at `seek_key` so no root is visited twice.
            loop {
                while let Some((data_root, entry)) = chunk_walk.next().transpose()? {
                    if wrapped && data_root >= seek_key {
                        break;
                    }
                    if entry.meta.updated_at >= delete_chunks_older_than {
                        tally.too_young += 1;
                        continue;
                    }
                    if candidates.len() >= MAX_EVICTIONS_PER_RUN
                        && !candidates.contains_key(&data_root)
                    {
                        warn!(
                            chunk.candidates = candidates.len(),
                            "Hit max eviction limit collecting prune candidates, will continue next cycle"
                        );
                        break;
                    }

                    candidates.entry(data_root).or_default().insert(entry.index);
                }
                if wrapped || candidates.len() >= MAX_EVICTIONS_PER_RUN {
                    break;
                }
                wrapped = true;
                chunk_walk = chunk_cursor.walk(None)?;
            }
            drop(chunk_walk);
            drop(chunk_cursor);
            drop(tx);
        }

        let (proven_roots, root_migration) = self.db.view_cache_eyre(|tx| {
            let consensus_tx = self.db.tx()?;
            let mut proven = BTreeSet::new();
            let mut migration = HashMap::with_capacity(candidates.len());
            for root in candidates.keys() {
                if Self::has_local_ingress_proof(tx.inner(), *root, local_address)? {
                    proven.insert(*root);
                }
                migration.insert(
                    *root,
                    Self::root_migration(tx, &consensus_tx, *root, migration_height)?,
                );
            }
            Ok((proven, migration))
        })?;

        // Resolve placement and durability outside the write txn: each root
        // costs one submodule index read per storage module.
        let storage_modules = self.storage_modules_guard.read().clone();
        let mut durable_by_root: Vec<(DataRoot, BTreeSet<TxChunkOffset>)> =
            Vec::with_capacity(candidates.len());
        let mut unprotected_by_root: Vec<(DataRoot, BTreeSet<TxChunkOffset>)> = Vec::new();
        for (root, cached_offsets) in candidates {
            let mut durable = BTreeSet::new();
            let mut placed = BTreeSet::new();
            let mut unreadable = false;
            irys_domain::trace_index_read(irys_domain::CACHE_RECLAIM, || {
                let proven = proven_roots.contains(&root);
                for storage_module in &storage_modules {
                    let read = storage_module
                        .durable_tx_offsets_for_data_root(root, &cached_offsets)
                        .and_then(|d| {
                            // Placement only decides which unproven bodies this
                            // node does not store. A local proof already keeps
                            // those bodies, so the second index read is wasted.
                            if proven {
                                return Ok((d, BTreeSet::new()));
                            }
                            let p = storage_module
                                .placed_tx_offsets_for_data_root(root, &cached_offsets)?;
                            Ok((d, p))
                        });
                    match read {
                        Ok((d, p)) => {
                            durable.extend(d);
                            placed.extend(p);
                        }
                        Err(error) => {
                            // Unreadable placement metadata is evidence of
                            // neither durability nor non-assignment; leave the
                            // bodies for a later pass.
                            unreadable = true;
                            warn!(
                                chunk.data_root = ?root,
                                storage_module = %storage_module.id,
                                ?error,
                                "Skipping chunk prune candidate with unreadable storage metadata"
                            );
                        }
                    }
                }
            });
            tally.candidates += cached_offsets.len() as u64;
            let not_durable = cached_offsets.difference(&durable).count() as u64;
            let proven = proven_roots.contains(&root);
            let migration = root_migration
                .get(&root)
                .copied()
                .unwrap_or(RootMigration::NoRecord);
            let migrated = migration == RootMigration::Migrated;
            if unreadable {
                tally.unreadable += not_durable;
            } else if proven {
                tally.not_durable += not_durable;
            } else if !migrated {
                tally.root_not_migrated += not_durable;
                match migration {
                    RootMigration::NoRecord => tally.root_no_record += not_durable,
                    RootMigration::NoTxids => tally.root_no_txids += not_durable,
                    RootMigration::TxPending | RootMigration::Migrated => {}
                }
                tally.root_not_migrated_placed += cached_offsets
                    .iter()
                    .filter(|offset| !durable.contains(offset) && placed.contains(offset))
                    .count() as u64;
            }
            if !unreadable && !proven && migrated {
                let mut unprotected: BTreeSet<TxChunkOffset> =
                    cached_offsets.difference(&placed).copied().collect();
                if !unprotected.is_empty() {
                    let keep = self.assigned_unpersisted_offsets(
                        root,
                        &unprotected,
                        &storage_modules,
                        migration_height,
                    )?;
                    unprotected.retain(|offset| !keep.contains(offset));
                }
                // Placed here, or assigned here and not yet persisted `Data`.
                let unprotected_not_durable = unprotected
                    .iter()
                    .filter(|offset| !durable.contains(offset))
                    .count() as u64;
                tally.not_durable += not_durable - unprotected_not_durable;
                if !unprotected.is_empty() {
                    tally.unplaced_attempted += unprotected.len() as u64;
                    unprotected_by_root.push((root, unprotected));
                }
            }
            if !durable.is_empty() {
                durable_by_root.push((root, durable));
            }
        }

        // The snapshot above can go stale before this delete. A checksum
        // mismatch sets the live interval to Uninitialized. Re-read each
        // batch here, still outside the MDBX write: `is_data_chunk_persisted_at`
        // takes the interval-disk lock and must not hold the cache writer.
        #[cfg(test)]
        if !durable_by_root.is_empty() {
            self.wait_if_prune_recheck_paused();
        }
        let mut pruned_chunks = 0_u64;
        for batch in durable_by_root.chunks(256) {
            let confirmed = self.recheck_durable_chunk_offsets(&storage_modules, batch);
            let offered: usize = batch.iter().map(|(_, offsets)| offsets.len()).sum();
            let still_durable: usize = confirmed.iter().map(|(_, offsets)| offsets.len()).sum();
            tally.not_durable += offered.saturating_sub(still_durable) as u64;
            if confirmed.is_empty() {
                continue;
            }
            let batch_counts = self
                .db
                .update_cache_eyre_at("cache_service.prune_chunks_batch", |write_tx| {
                    let mut batch_counts = irys_database::ReclaimCounts::default();
                    for (root, durable_tx_offsets) in &confirmed {
                        trace!(
                            chunk.data_root = ?root,
                            "Pruning durably stored cached chunk bodies"
                        );
                        let counts = reclaim_cached_chunks_older_than(
                            write_tx.inner(),
                            *root,
                            local_address,
                            durable_tx_offsets,
                            delete_chunks_older_than,
                        )?;
                        if counts.deleted > 0 {
                            debug!(chunk.data_root = ?root, chunk.pruned_chunks = counts.deleted, "Pruned durably stored cached chunk bodies");
                        }
                        batch_counts.deleted += counts.deleted;
                        batch_counts.too_young += counts.too_young;
                        batch_counts.no_ingress_hash += counts.no_ingress_hash;
                    }
                    Ok(batch_counts)
                })?;
            // Count only deletes whose enclosing MDBX transaction committed.
            pruned_chunks = pruned_chunks.saturating_add(batch_counts.deleted);
            tally.deleted_durable += batch_counts.deleted;
            tally.too_young += batch_counts.too_young;
            tally.no_ingress_hash += batch_counts.no_ingress_hash;
        }

        for batch in unprotected_by_root.chunks(256) {
            let still = Self::recheck_unplaced_chunk_offsets(&storage_modules, batch);
            let mut deletable = Vec::with_capacity(still.len());
            for (root, offsets) in still {
                let keep = self.assigned_unpersisted_offsets(
                    root,
                    &offsets,
                    &storage_modules,
                    migration_height,
                )?;
                let remaining: BTreeSet<TxChunkOffset> =
                    offsets.difference(&keep).copied().collect();
                if !remaining.is_empty() {
                    deletable.push((root, remaining));
                }
            }
            if deletable.is_empty() {
                continue;
            }
            let batch_pruned = self
                .db
                .update_cache_eyre_at("cache_service.prune_unprotected_chunks_batch", |write_tx| {
                    let mut batch_pruned = 0_u64;
                    for (root, offsets) in &deletable {
                        // A proof generated since the scan protects the root.
                        if Self::has_local_ingress_proof(write_tx.inner(), *root, local_address)? {
                            continue;
                        }
                        let pruned = delete_unprotected_cached_chunks_older_than(
                            write_tx.inner(),
                            *root,
                            offsets,
                            delete_chunks_older_than,
                        )?;
                        if pruned > 0 {
                            batch_pruned = batch_pruned.saturating_add(pruned);
                            debug!(chunk.data_root = ?root, chunk.pruned_chunks = pruned, "Pruned cached chunk bodies this node does not store");
                        }
                    }
                    Ok(batch_pruned)
                })?;
            pruned_chunks = pruned_chunks.saturating_add(batch_pruned);
            tally.deleted_unplaced += batch_pruned;
        }

        info!(
            chunk.pruned_chunks = pruned_chunks,
            prune.candidates = tally.candidates,
            prune.deleted_durable = tally.deleted_durable,
            prune.deleted_unplaced = tally.deleted_unplaced,
            prune.kept_too_young = tally.too_young,
            prune.kept_not_durable = tally.not_durable,
            prune.kept_no_ingress_hash = tally.no_ingress_hash,
            prune.kept_root_not_migrated = tally.root_not_migrated,
            prune.root_no_record = tally.root_no_record,
            prune.root_no_txids = tally.root_no_txids,
            prune.root_not_migrated_placed = tally.root_not_migrated_placed,
            prune.kept_unreadable = tally.unreadable,
            prune.kept_unplaced = tally.unplaced_kept(),
            "Completed chunk pruning pass"
        );
        metrics::record_cache_prune_pass(&tally);
        Ok(tally)
    }

    /// Offsets from `batch` that still have no local placement. A placement
    /// made since the scan, or an unreadable module, keeps the body.
    fn recheck_unplaced_chunk_offsets(
        storage_modules: &[Arc<irys_domain::StorageModule>],
        batch: &[(DataRoot, BTreeSet<TxChunkOffset>)],
    ) -> Vec<(DataRoot, BTreeSet<TxChunkOffset>)> {
        let mut still = Vec::with_capacity(batch.len());
        'roots: for (root, offsets) in batch {
            let mut remaining = offsets.clone();
            for storage_module in storage_modules {
                match storage_module.placed_tx_offsets_for_data_root(*root, offsets) {
                    Ok(placed) => remaining.retain(|offset| !placed.contains(offset)),
                    Err(_) => continue 'roots,
                }
            }
            if !remaining.is_empty() {
                still.push((*root, remaining));
            }
        }
        still
    }

    /// Offsets from `batch` that are still persisted. A storage-module error
    /// drops that module's offsets from the delete set and keeps the bodies.
    fn recheck_durable_chunk_offsets(
        &self,
        storage_modules: &[Arc<irys_domain::StorageModule>],
        batch: &[(DataRoot, BTreeSet<TxChunkOffset>)],
    ) -> Vec<(DataRoot, BTreeSet<TxChunkOffset>)> {
        let mut confirmed = Vec::with_capacity(batch.len());
        for (root, offsets) in batch {
            let mut still = BTreeSet::new();
            for storage_module in storage_modules {
                match storage_module.durable_tx_offsets_for_data_root(*root, offsets) {
                    Ok(live) => still.extend(live),
                    Err(error) => {
                        warn!(
                            chunk.data_root = ?root,
                            storage_module = %storage_module.id,
                            ?error,
                            "keeping cached chunk bodies; durability re-check failed"
                        );
                    }
                }
            }
            if !still.is_empty() {
                confirmed.push((*root, still));
            }
        }
        confirmed
    }

    /// Offsets in `candidates` that a local module is still assigned to store
    /// and that are not this chunk's persisted `Data`.
    ///
    /// Open drops the placement row when the file is not `Data`. The partition
    /// assignment and the canonical ledger range stay. Keep the cached body
    /// until the file says `Data` and the index does not name another chunk
    /// there. A missing block or tx header skips that span. A database error
    /// fails the prune.
    fn assigned_unpersisted_offsets(
        &self,
        data_root: DataRoot,
        candidates: &BTreeSet<TxChunkOffset>,
        storage_modules: &[Arc<irys_domain::StorageModule>],
        migration_height: u64,
    ) -> eyre::Result<BTreeSet<TxChunkOffset>> {
        let mut keep = BTreeSet::new();
        if candidates.is_empty()
            || storage_modules.iter().all(|sm| {
                sm.partition_assignment()
                    .is_none_or(|a| a.ledger_id.is_none() || a.slot_index.is_none())
            })
        {
            return Ok(keep);
        }
        let spans = self.ledger_spans_for_root(data_root, migration_height)?;
        if spans.is_empty() {
            return Ok(keep);
        }
        for tx_offset in candidates {
            let raw = u64::from(**tx_offset);
            let mut keep_offset = false;
            'spans: for span in &spans {
                if raw >= span.chunk_count {
                    continue;
                }
                let Some(ledger_offset) = checked_ledger_offset(span.start, raw) else {
                    continue;
                };
                for sm in storage_modules {
                    let Some(assignment) = sm.partition_assignment() else {
                        continue;
                    };
                    let (Some(ledger_id), Some(slot_index)) =
                        (assignment.ledger_id, assignment.slot_index)
                    else {
                        continue;
                    };
                    if ledger_id != u32::from(span.ledger) {
                        continue;
                    }
                    // `contains_offset` and the helper multiply `slot * num_chunks`
                    // in `u64`. Debug builds panic on overflow. Release builds wrap.
                    // A product that does not fit is not this slot.
                    if (slot_index as u64)
                        .checked_mul(sm.config.consensus.num_chunks_in_partition)
                        .is_none()
                    {
                        continue;
                    }
                    if !sm.contains_offset(ledger_offset) {
                        continue;
                    }
                    let Ok(partition_raw) =
                        sm.make_offset_partition_relative_guarded(ledger_offset)
                    else {
                        continue;
                    };
                    let partition_offset = PartitionChunkOffset::from(partition_raw);
                    // One placement point read. A span view would walk every
                    // interval in the partition for a single candidate.
                    let indexed = sm.data_root_and_tx_offset_at(partition_offset)?;
                    let names_this_chunk = indexed
                        .is_some_and(|(root, tx_off)| root == data_root && tx_off == *tx_offset);
                    // A stored placement for a different chunk means the Data
                    // label is that chunk's bytes, not this one's.
                    if indexed.is_some() && !names_this_chunk {
                        keep_offset = true;
                        break 'spans;
                    }
                    if !sm.is_data_chunk_persisted_at(partition_offset) {
                        keep_offset = true;
                        break 'spans;
                    }
                }
            }
            if keep_offset {
                keep.insert(*tx_offset);
            }
        }
        Ok(keep)
    }

    /// Ledger spans this root's txs occupy at or below `migration_height`.
    ///
    /// A Publish-targeted tx is included on Submit, so `included_height` is
    /// that Submit block. Promotion adds a separate Publish span.
    /// Term-ledger txs are placed on their own ledger at `included_height`.
    fn ledger_spans_for_root(
        &self,
        data_root: DataRoot,
        migration_height: u64,
    ) -> eyre::Result<Vec<TxLedgerSpan>> {
        let headers = self.db.view_cache_eyre(|tx| {
            let Some(cached) = tx.get::<CachedDataRoots>(data_root)? else {
                return Ok(Vec::new());
            };
            let consensus_tx = self.db.tx()?;
            let mut headers = Vec::with_capacity(cached.txid_set.len());
            for tx_id in &cached.txid_set {
                if let Some(header) = tx_header_by_txid(&consensus_tx, tx_id)? {
                    headers.push(header);
                }
            }
            Ok(headers)
        })?;
        let mut spans = Vec::new();
        for header in &headers {
            for (ledger, height) in Self::inclusion_placements(header, migration_height) {
                if let Some(span) = self.tx_span_in_block(header.id, ledger, height)? {
                    spans.push(span);
                }
            }
        }
        Ok(spans)
    }

    /// `(ledger, height)` pairs whose chunk range this tx occupies.
    fn inclusion_placements(
        header: &DataTransactionHeader,
        migration_height: u64,
    ) -> Vec<(DataLedger, u64)> {
        let mut placements = Vec::new();
        if let Some(height) = header.metadata().included_height
            && height <= migration_height
        {
            match DataLedger::try_from(header.ledger_id) {
                // Publish-targeted data is staged on Submit. `included_height`
                // is that Submit block, not a Publish block.
                Ok(DataLedger::Submit | DataLedger::Publish) => {
                    placements.push((DataLedger::Submit, height));
                }
                Ok(ledger @ (DataLedger::OneYear | DataLedger::ThirtyDay)) => {
                    placements.push((ledger, height));
                }
                Err(_) => {}
            }
        }
        if let Some(height) = header.promoted_height()
            && height <= migration_height
        {
            placements.push((DataLedger::Publish, height));
        }
        placements
    }

    /// The tx's span inside `ledger` on the canonical block at `height`.
    ///
    /// `None` when the block, a prior tx header, or the tx itself is missing.
    /// Guessing a start of 0 would assign the chunk to slot 0.
    fn tx_span_in_block(
        &self,
        tx_id: H256,
        ledger: DataLedger,
        height: u64,
    ) -> eyre::Result<Option<TxLedgerSpan>> {
        let Some(block) = self.onchain_header_at_height(height)? else {
            return Ok(None);
        };
        let Some(start) = self.ledger_chunk_start(&block, ledger)? else {
            return Ok(None);
        };
        let Some(tx_ids) = block.get_data_ledger_tx_ids_ordered(ledger) else {
            return Ok(None);
        };
        let chunk_size = self.config.consensus.chunk_size;
        self.db.view_eyre(|tx| {
            let mut cursor = start;
            for id in tx_ids {
                let Some(header) = tx_header_by_txid(tx, id)? else {
                    return Ok(None);
                };
                let count = header.data_size.div_ceil(chunk_size);
                if *id == tx_id {
                    if count == 0 {
                        return Ok(None);
                    }
                    return Ok(Some(TxLedgerSpan {
                        ledger,
                        start: cursor,
                        chunk_count: count,
                    }));
                }
                let Some(next) = checked_ledger_offset(cursor, count) else {
                    return Ok(None);
                };
                cursor = next;
            }
            Ok(None)
        })
    }

    /// Ledger offset where `block` starts appending to `ledger`.
    ///
    /// Height 0 starts at 0. A missing previous header is `None` (do not
    /// guess 0). A previous block that has no such ledger started it here.
    fn ledger_chunk_start(
        &self,
        block: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> eyre::Result<Option<LedgerChunkOffset>> {
        if block.height == 0 {
            return Ok(Some(LedgerChunkOffset::from(0_u64)));
        }
        let Some(prev) = self.onchain_header_at_height(block.height - 1)? else {
            return Ok(None);
        };
        Ok(Some(LedgerChunkOffset::from(
            prev.ledger_total_chunks(ledger),
        )))
    }

    /// Canonical header at `height`: the in-memory on-chain tree, else the
    /// migrated-block tables. The tree guard is dropped before the database read.
    fn onchain_header_at_height(&self, height: u64) -> eyre::Result<Option<IrysBlockHeader>> {
        let from_tree = {
            let tree = self.block_tree_guard.read();
            let (chain, _) = tree.get_canonical_chain();
            chain
                .iter()
                .find(|entry| entry.height() == height)
                .and_then(|entry| {
                    let (header, state) = tree.get_block_and_status(&entry.block_hash())?;
                    (*state == ChainState::Onchain).then(|| header.clone())
                })
        };
        if let Some(header) = from_tree {
            return Ok(Some(header));
        }
        self.db
            .view_eyre(|tx| irys_database::canonical_header_at_height(tx, height))
    }

    #[cfg(test)]
    fn arm_prune_recheck_pause(&self) {
        let pause = &self.prune_recheck_pause;
        *pause.release.lock().unwrap() = false;
        *pause.arrived.lock().unwrap() = false;
        *pause.armed.lock().unwrap() = true;
    }

    #[cfg(test)]
    fn wait_for_prune_recheck_pause(&self, timeout: std::time::Duration) -> bool {
        let pause = &self.prune_recheck_pause;
        let guard = pause.arrived.lock().unwrap();
        let (guard, _) = pause
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| !*arrived)
            .unwrap();
        *guard
    }

    #[cfg(test)]
    fn release_prune_recheck_pause(&self) {
        let pause = &self.prune_recheck_pause;
        *pause.release.lock().unwrap() = true;
        pause.release_cv.notify_all();
    }

    #[cfg(test)]
    fn wait_if_prune_recheck_paused(&self) {
        let pause = &self.prune_recheck_pause;
        {
            let mut armed = pause.armed.lock().unwrap();
            if !*armed {
                return;
            }
            *armed = false;
        }
        {
            let mut arrived = pause.arrived.lock().unwrap();
            *arrived = true;
            pause.arrived_cv.notify_all();
        }
        let mut release = pause.release.lock().unwrap();
        while !*release {
            release = pause.release_cv.wait(release).unwrap();
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(prune_height))]
    fn prune_data_root_cache(&self, prune_height: u64) -> eyre::Result<()> {
        // Per-eviction observability state, buffered inside the write tx and
        // emitted only after `commit()` succeeds — otherwise a failed commit
        // (or a mid-loop delete error that drops the tx) would report
        // evictions/chunks that never persisted.
        struct EvictionRecord {
            data_root: H256,
            block_set: Vec<H256>,
            txid_set_size: usize,
            inclusion_max_height: Option<u64>,
            expiry_height: Option<u64>,
            horizon: Option<u64>,
        }

        let signer = self.config.irys_signer();
        let local_addr = signer.address();

        // Collect eviction candidates under a short RO txn (random seek + wraparound),
        // then apply deletes in gated write batches so the cache writer gate is
        // released between batches rather than held for the whole table scan.
        let mut candidates: Vec<EvictionRecord> = Vec::new();
        {
            let tx = ScopedTx::<Cache>::new(self.db.cache().tx()?);
            let consensus_tx = self.db.tx()?;
            let mut cursor = tx.cursor_read::<CachedDataRoots>()?;
            let seek_key: DataRoot = H256::random();
            let mut walker = cursor.walk(Some(seek_key))?;
            let mut wrapped = false;
            loop {
                while let Some((data_root, cached)) = walker.next().transpose()? {
                    // The wrap stops at the seek key so no root is visited twice.
                    if wrapped && data_root >= seek_key {
                        break;
                    }
                    if candidates.len() >= MAX_EVICTIONS_PER_RUN {
                        warn!(
                            candidates = candidates.len(),
                            "Hit max eviction limit collecting prune_data_root_cache candidates, will continue next cycle"
                        );
                        break;
                    }

                    let state = Self::cdr_eviction_state(&consensus_tx, &cached, prune_height)?;
                    if !state.evictable {
                        continue;
                    }
                    if Self::has_local_ingress_proof(tx.inner(), data_root, local_addr)? {
                        continue;
                    }

                    candidates.push(EvictionRecord {
                        data_root,
                        block_set: cached.block_set,
                        txid_set_size: cached.txid_set.len(),
                        inclusion_max_height: state.inclusion_max_height,
                        expiry_height: cached.expiry_height,
                        horizon: state.horizon,
                    });
                }
                if wrapped || candidates.len() >= MAX_EVICTIONS_PER_RUN {
                    break;
                }
                wrapped = true;
                walker = cursor.walk(None)?;
            }
            drop(walker);
            drop(cursor);
            drop(tx);
        }

        let mut chunks_pruned: u64 = 0;
        let mut applied: Vec<EvictionRecord> = Vec::new();
        for batch in candidates.chunks(256) {
            let mut batch_applied: Vec<EvictionRecord> = Vec::new();
            let mut batch_chunks: u64 = 0;
            self.db
                .update_cache_eyre_at("cache_service.prune_data_root_cache", |write_tx| {
                    let consensus_tx = self.db.tx()?;
                    for rec in batch {
                        // Re-validate under the write txn: the root may have been
                        // re-cached, gained a local proof, or left its eviction
                        // horizon between the RO scan and this gated write.
                        let Some(cached) = write_tx.get::<CachedDataRoots>(rec.data_root)? else {
                            continue;
                        };
                        let state = Self::cdr_eviction_state(&consensus_tx, &cached, prune_height)?;
                        if !state.evictable
                            || Self::has_local_ingress_proof(
                                write_tx.inner(),
                                rec.data_root,
                                local_addr,
                            )?
                        {
                            continue;
                        }
                        write_tx.delete::<IngressProofs>(rec.data_root, None)?;
                        batch_chunks = batch_chunks.saturating_add(
                            delete_cached_chunks_by_data_root(write_tx, rec.data_root)?,
                        );
                        irys_database::delete_ingress_leaves_by_data_root(
                            write_tx.inner(),
                            rec.data_root,
                        )?;
                        write_tx.delete::<CachedDataRoots>(rec.data_root, None)?;
                        // Fresh values from the write txn, not the scan snapshot,
                        // so post-commit logging reflects what was actually deleted.
                        batch_applied.push(EvictionRecord {
                            data_root: rec.data_root,
                            block_set: cached.block_set,
                            txid_set_size: cached.txid_set.len(),
                            inclusion_max_height: state.inclusion_max_height,
                            expiry_height: cached.expiry_height,
                            horizon: state.horizon,
                        });
                    }
                    Ok(())
                })?;
            chunks_pruned = chunks_pruned.saturating_add(batch_chunks);
            applied.extend(batch_applied);
        }

        // Post-commit: only report what actually persisted in a committed write batch.
        for entry in &applied {
            info!(
                data_root = %entry.data_root,
                block_set_size = entry.block_set.len(),
                txid_set_size = entry.txid_set_size,
                last_inclusion_height = ?entry.inclusion_max_height,
                expiry_height = ?entry.expiry_height,
                horizon = ?entry.horizon,
                %prune_height,
                has_local_proof = false,
                "cached_data_root.evict"
            );
            debug!(
                data_root = %entry.data_root,
                block_set = ?entry.block_set,
                "cached_data_root.evict.block_set"
            );
            metrics::record_cached_data_root_evicted();
        }
        debug!(data_root.chunks_pruned = ?chunks_pruned, "Pruned chunks");

        Ok(())
    }

    /// Background scrub of `CachedDataRoot.txid_set` after mempool prune / reorg.
    /// Keeps any txid that is migrated (`IrysDataTxMetadata`) or still in the
    /// canonical tree window (confirmed but not yet migrated).
    fn spawn_prune_txids_task(&self, by_data_root: HashMap<H256, Vec<H256>>) {
        let clone = self.clone();
        std::thread::spawn(move || {
            // catch_unwind so a panic inside the body still drives a
            // `PruneTxidsCompleted` back to the actor; without it the
            // `txid_prune_running` flag stays `true` forever and the queue
            // jams permanently.  `AssertUnwindSafe` is sound here — the
            // captured `clone` is `Send + Clone`, and on the panic path we
            // immediately stop using it after the catch.
            let result: eyre::Result<()> =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    clone.prune_txids_from_cached_data_roots(&by_data_root)
                })) {
                    Ok(r) => r,
                    Err(payload) => {
                        let msg = panic_payload_string(payload);
                        error!(custom.panic = %msg, "spawn_prune_txids_task body panicked");
                        Err(eyre::eyre!("spawn_prune_txids_task panicked: {msg}"))
                    }
                };

            let completion = match &result {
                Ok(_) => Ok(()),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            };
            if let Err(e) = result {
                warn!("Failed to prune stale txids from CachedDataRoots: {}", e);
            }
            if let Err(e) = clone
                .cache_sender
                .send_traced(CacheServiceAction::PruneTxidsCompleted(completion))
            {
                warn!(custom.error = ?e, "Failed to notify PruneTxidsCompleted");
            }
        });
    }

    /// Retrying scrub body: one-shot batches must not be dropped on tree-lock
    /// deferral (stale `txid_set` entries would pin data roots).
    fn prune_txids_from_cached_data_roots(
        &self,
        by_data_root: &HashMap<H256, Vec<H256>>,
    ) -> eyre::Result<()> {
        let mut attempts = 0_u64;
        loop {
            match self.try_prune_txids_once(by_data_root)? {
                TxidScrubAttempt::Done => return Ok(()),
                TxidScrubAttempt::Deferred => {
                    attempts += 1;
                    // 100 × 50 ms — surface sustained contention every ~5 s
                    // without giving up (abandoning the batch pins the cache).
                    if attempts.is_multiple_of(100) {
                        warn!(
                            attempts,
                            "txid scrub still deferred by block-tree lock contention; retrying"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    /// One scrub attempt; [`TxidScrubAttempt::Deferred`] if the tree is write-held.
    fn try_prune_txids_once(
        &self,
        by_data_root: &HashMap<H256, Vec<H256>>,
    ) -> eyre::Result<TxidScrubAttempt> {
        self.db.update_cache_eyre(|db_tx| {
            // Snapshot canonical tree txids after taking the cache writer lock.
            // try_read only: blocking
            // would invert lock order with deep-reorg (tree-write → DB write).
            let tree_set: HashSet<H256> = {
                let tree = match self.block_tree_guard.try_read() {
                    Ok(tree) => tree,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        debug!(
                            "block tree lock write-held during the txid scrub; deferring this attempt"
                        );
                        return Ok(TxidScrubAttempt::Deferred);
                    }
                    // A poisoned tree lock is a node fault, not contention:
                    // deferring would spin the retry loop forever behind a
                    // misleading "write-held" log. Surface it through the
                    // worker's `PruneTxidsCompleted(Err(..))` path instead.
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(eyre::eyre!(
                            "block tree lock poisoned during the txid scrub"
                        ));
                    }
                };
                let (canonical, _) = tree.get_canonical_chain();
                drop(tree);
                let mut set = HashSet::new();
                for entry in &canonical {
                    for tx_ids in entry.header().get_data_ledger_tx_ids()?.into_values() {
                        set.extend(tx_ids);
                    }
                }
                set
            };

            // Read metadata after the tree snapshot so migration from the tree
            // into consensus storage cannot leave a tx absent from both views.
            let consensus_tx = self.db.tx()?;
            for (data_root, txids_to_remove) in by_data_root {
                let Some(mut cached) = db_tx.get::<CachedDataRoots>(*data_root)? else {
                    continue;
                };

                // Build the actually-safe-to-remove set: keep any tx_id that
                // has been (re-)confirmed since the scrub was queued. A tx is
                // (re-)confirmed if its metadata row exists (migrated) OR it
                // appears in the canonical tree snapshot (confirmed but not yet
                // migrated) — see the race rationale in `spawn_prune_txids_task`.
                let mut removal_set: HashSet<H256> = HashSet::new();
                let mut raced_confirmed: usize = 0;
                for tx_id in txids_to_remove {
                    if get_data_tx_metadata(&consensus_tx, tx_id)?.is_some() || tree_set.contains(tx_id) {
                        raced_confirmed += 1;
                        continue;
                    }
                    removal_set.insert(*tx_id);
                }
                if raced_confirmed > 0 {
                    debug!(
                        data_root = %data_root,
                        raced_confirmed,
                        "Skipped txid scrub for tx(es) (re-)confirmed during scrub window"
                    );
                }
                if removal_set.is_empty() {
                    continue;
                }

                let before_len = cached.txid_set.len();
                cached.txid_set.retain(|id| !removal_set.contains(id));

                if cached.txid_set.len() < before_len {
                    debug!(
                        data_root = %data_root,
                        removed = before_len - cached.txid_set.len(),
                        remaining = cached.txid_set.len(),
                        "Pruned stale txids from CachedDataRoot.txid_set"
                    );
                    // Mirrors the `(None, None)` warn in
                    // `remove_data_root_block_set_entry`: draining the
                    // last txid while `expiry_height` is already None
                    // leaves the CDR vacuously eviction-eligible (only
                    // the local-proof exemption keeps it alive).
                    // Surface this transition so observability is
                    // symmetric across the two paths that produce it.
                    if cached.txid_set.is_empty() && cached.expiry_height.is_none() {
                        warn!(
                            data_root = %data_root,
                            "CachedDataRoot left in anomalous (no expiry, empty txid_set) state \
                             after stale-txid prune; entry now eviction-eligible unless held by \
                             a local ingress proof"
                        );
                    }
                    db_tx.put::<CachedDataRoots>(*data_root, cached)?;
                }
            }
            Ok(TxidScrubAttempt::Done)
        })
    }

    /// Scans ingress proofs with capacity-aware deletion and regeneration:
    /// - (a) Promoted + expired: delete
    /// - (b) Unpromoted + expired + at capacity: delete, except a proof this
    ///   node produced while a cached body for that root is still unpersisted.
    ///   That proof is reanchored. The cap is for chunk bytes; the proof row
    ///   is what keeps those bytes servable.
    /// - (c) Unpromoted + expired + under capacity + local author: regenerate
    #[tracing::instrument(level = "trace", skip_all)]
    fn prune_ingress_proofs(&self) -> eyre::Result<()> {
        // A stable prefix of still-valid proofs must not consume the budget
        // on every pass. Start at a random key and wrap once. The cap stays.
        self.prune_ingress_proofs_limited(H256::random(), MAX_PROOF_CHECKS_PER_RUN)
    }

    /// Scan at most `max_checks` proofs, starting at `seek_key` and wrapping
    /// once. Every record counts, valid or not. The wrap stops at `seek_key`
    /// so a proof is not visited twice.
    fn prune_ingress_proofs_limited(
        &self,
        seek_key: DataRoot,
        max_checks: usize,
    ) -> eyre::Result<()> {
        let signer = self.config.irys_signer();
        let local_addr = signer.address();
        let tx = ScopedTx::<Cache>::new(self.db.cache().tx()?);
        let consensus_tx = self.db.tx()?;
        let mut cursor = tx.cursor_read::<IngressProofs>()?;
        let mut walker = cursor.walk(Some(seek_key))?;
        let mut wrapped = false;
        // Carries the full proof content so the delete phase can compare before deleting
        // (closes the TOCTOU window: a fresh proof stored between the scan and the delete
        // will not match the scanned content and will be preserved). The bool flags an
        // orphan deletion (no `CachedDataRoots` entry at scan time); for those the delete
        // phase also re-checks the root is still absent, so a root re-cached between scan
        // and delete keeps its proof.
        let mut to_delete: Vec<(DataRoot, CompactCachedIngressProof, bool)> = Vec::new();
        let mut to_reanchor: Vec<IngressProof> = Vec::new();
        let mut to_regen: Vec<IngressProof> = Vec::new();
        let mut processed = 0_usize;
        let max_cache_size_bytes = &self.config.node_config.cache.max_cache_size_bytes;

        // Determine if cache is at capacity based on the CachedChunks table
        let (chunk_count, chunk_cache_size) =
            get_cache_size::<CachedChunks, _>(tx.inner(), self.config.consensus.chunk_size)?;
        let at_capacity = chunk_cache_size >= *max_cache_size_bytes;
        debug!(
            "Cache is {} at capacity - capacity: {}B, size {}B ({} chunks)",
            if at_capacity { "" } else { "not" },
            &max_cache_size_bytes,
            chunk_cache_size,
            &chunk_count
        );

        loop {
            while let Some((data_root, compact)) = walker.next().transpose()? {
                if wrapped && data_root >= seek_key {
                    break;
                }
                if processed >= max_checks {
                    break;
                }
                processed += 1;
                let CachedIngressProof { address, proof } = compact.0.clone();

                // Associated txids
                let Some(cached_data_root) = cached_data_root_by_data_root(&tx, data_root)? else {
                    debug!(ingress_proof.data_root = ?data_root, "Proof has no cached data root; marking for deletion");
                    to_delete.push((data_root, compact, true));
                    continue;
                };

                let check_result = ChunkIngressServiceInner::is_ingress_proof_expired_static(
                    &self.block_tree_guard,
                    &self.db,
                    &self.config,
                    &proof,
                );
                if !check_result.expired_or_invalid {
                    continue;
                }
                let mut any_unpromoted = false;
                for txid in cached_data_root.txid_set.iter() {
                    if let Some(tx_header) = tx_header_by_txid(&consensus_tx, txid)?
                        && tx_header.promoted_height().is_none()
                    {
                        any_unpromoted = true;
                        debug!(ingress_proof.data_root = ?data_root, tx.id = ?tx_header.id, "Found unpromoted tx for data root");
                        break;
                    }
                }

                let is_locally_produced = address == local_addr;

                if at_capacity {
                    // A local unpromoted proof is the fence for cached bodies
                    // no module has persisted. Deleting it in this pass makes
                    // the body prune treat those bytes as unprotected.
                    if is_locally_produced
                        && any_unpromoted
                        && self.cached_body_still_unpersisted(tx.inner(), data_root)?
                    {
                        match check_result.regeneration_action {
                            RegenAction::Reanchor => {
                                to_reanchor.push(proof);
                                debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = true, "Marking expired local proof for reanchoring; cached bodies are unpersisted");
                            }
                            RegenAction::Regenerate => {
                                to_regen.push(proof);
                                debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = true, "Marking expired local proof for full regeneration; cached bodies are unpersisted");
                            }
                            RegenAction::DoNotRegenerate => {
                                error!(
                                    "Cache is at capacity and the local proof for data root {} is expired with unpromoted txs and unpersisted cached bodies, but it does not meet reanchoring or regeneration criteria. Leaving the proof in place.",
                                    &data_root
                                );
                            }
                        }
                    } else {
                        to_delete.push((data_root, compact, false));
                        debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = true, "Marking expired proof for deletion (at capacity)");
                    }
                } else if is_locally_produced && any_unpromoted {
                    match check_result.regeneration_action {
                        RegenAction::Reanchor => {
                            to_reanchor.push(proof);
                            debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = false, "Marking expired local proof for reanchoring");
                        }
                        RegenAction::Regenerate => {
                            to_regen.push(proof);
                            debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = false, "Marking expired local proof for full regeneration");
                        }
                        RegenAction::DoNotRegenerate => {
                            error!(
                                "We're under capacity, and the proof is expired and local with unpromoted txs, but proof with data root {} does not meet reanchoring or regeneration criteria. This should not happen.",
                                &data_root
                            );
                        }
                    }
                } else {
                    // Not local + expired: delete
                    to_delete.push((data_root, compact, false));
                    debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = false, "Marking expired proof for deletion (remote or fully-promoted)");
                }
            }
            if wrapped || processed >= max_checks {
                break;
            }
            wrapped = true;
            walker = cursor.walk(None)?;
        }

        // Release the scan's read transaction (and its walker/cursor) before the
        // per-proof write-tx loop below. Holding a long-lived reader open across
        // up to MAX_PROOF_CHECKS_PER_RUN short write txns would pin the MVCC
        // snapshot and hold back MDBX free-page reclamation; nothing past the scan
        // reads from `tx` (the delete/reanchor/regen loops open their own txns).
        drop(walker);
        drop(cursor);
        drop(tx);
        drop(consensus_tx);

        // Delete expired proofs — per-signer to avoid wiping other signers' rows on the same
        // data_root. Content-aware delete closes the TOCTOU window: a fresh proof stored
        // between the scan and this delete will not match the scanned value and is preserved.
        if !to_delete.is_empty() {
            let mut deleted_count = 0_usize;
            for (root, scanned, is_orphan) in to_delete.iter() {
                match self.db.update_cache_eyre(|rw_tx| {
                    // Orphan deletions were scheduled because the data root had no
                    // `CachedDataRoots` entry at scan time. Re-check inside the write tx:
                    // if the root was re-cached since the scan, keep its proof.
                    if *is_orphan && rw_tx.get::<CachedDataRoots>(*root)?.is_some() {
                        return Ok(false);
                    }
                    delete_ingress_proof_if_unchanged(rw_tx, *root, scanned.clone())
                }) {
                    Ok(true) => deleted_count += 1,
                    Ok(false) => {
                        debug!(
                            ingress_proof.data_root = ?root,
                            "Skipping stale delete: proof was refreshed or data root re-cached since scan"
                        );
                    }
                    Err(e) => {
                        warn!(ingress_proof.data_root = ?root, "Failed to remove ingress proof: {e}");
                    }
                }
            }
            if deleted_count > 0 {
                info!(
                    proofs.deleted = deleted_count,
                    "Deleted expired ingress proofs"
                );
            }
        }

        // Regenerate local expired proofs (only when under capacity)
        for proof in to_reanchor.iter() {
            if let Err(error) = reanchor_and_store_ingress_proof(
                &self.block_tree_guard,
                &self.db,
                &self.config,
                &signer,
                proof,
                &self.gossip_broadcast,
                &self.ingress_proof_generation_state,
            ) {
                if error.is_benign() {
                    debug!(ingress_proof.data_root = ?proof, "Skipped ingress proof reanchoring: {error}");
                } else {
                    warn!(ingress_proof.data_root = ?proof, "Failed to regenerate ingress proof: {error}");
                }
            }
        }

        for proof in to_regen.iter() {
            if let Err(error) = generate_and_store_ingress_proof(
                &self.block_tree_guard,
                &self.db,
                &self.config,
                proof.data_root,
                None,
                &self.gossip_broadcast,
                &self.ingress_proof_generation_state,
            ) {
                if error.is_benign() {
                    debug!(ingress_proof.data_root = ?proof.data_root, "Skipped ingress proof regeneration: {error}");
                } else {
                    warn!(ingress_proof.data_root = ?proof.data_root, "Failed to regenerate ingress proof: {error}");
                }
            }
        }

        if !to_reanchor.is_empty() {
            info!(
                proofs.regenerated = to_reanchor.len(),
                "Reanchored expired local ingress proofs"
            );
        }

        Ok(())
    }

    /// True when `data_root` still has a cached body that no storage module
    /// reports as persisted `Data`. An unreadable module is not that report:
    /// the proof stays, and a later pass can delete it once the read succeeds
    /// and every cached offset is persisted.
    fn cached_body_still_unpersisted<T: reth_db::transaction::DbTx>(
        &self,
        tx: &T,
        data_root: DataRoot,
    ) -> eyre::Result<bool> {
        use reth_db::cursor::DbDupCursorRO as _;

        let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
        let mut walker = cursor.walk_dup(Some(data_root), None)?;
        let mut offsets = BTreeSet::new();
        while let Some((key, entry)) = walker.next().transpose()? {
            if key != data_root {
                break;
            }
            offsets.insert(entry.index);
        }
        drop(walker);
        drop(cursor);
        if offsets.is_empty() {
            return Ok(false);
        }

        let modules = self.storage_modules_guard.read().clone();
        let mut persisted = BTreeSet::new();
        for storage_module in &modules {
            match storage_module.durable_tx_offsets_for_data_root(data_root, &offsets) {
                Ok(durable) => persisted.extend(durable),
                Err(error) => {
                    warn!(
                        chunk.data_root = ?data_root,
                        storage_module = %storage_module.id,
                        ?error,
                        "Keeping local ingress proof: storage metadata is unreadable"
                    );
                    return Ok(true);
                }
            }
        }
        Ok(offsets.iter().any(|offset| !persisted.contains(offset)))
    }

    fn spawn_pruning_task(
        &self,
        migration_height: u64,
        response_sender: Option<oneshot::Sender<eyre::Result<()>>>,
    ) {
        let clone = self.clone();
        std::thread::spawn(move || {
            // Same panic-survival rationale as `spawn_prune_txids_task`:
            // without `catch_unwind`, a panic anywhere in `prune_cache`
            // leaves `pruning_running = true` forever and waiters of
            // `response_sender` hang.
            let res: eyre::Result<()> =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    clone.prune_cache(migration_height)
                })) {
                    Ok(r) => r,
                    Err(payload) => {
                        let msg = panic_payload_string(payload);
                        error!(custom.panic = %msg, "spawn_pruning_task body panicked");
                        Err(eyre::eyre!("spawn_pruning_task panicked: {msg}"))
                    }
                };
            let completion = match &res {
                Ok(_) => Ok(()),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            };
            if let Some(sender) = response_sender
                && let Err(error) = sender.send(res)
            {
                warn!(custom.error = ?error, "RX failure for OnBlockMigrated");
            }
            // Notify service that pruning finished (drive the queue)
            if let Err(e) = clone
                .cache_sender
                .send_traced(CacheServiceAction::PruneCompleted(completion))
            {
                warn!(custom.error = ?e, "Failed to notify PruneCompleted");
            }
        });
    }

    fn spawn_epoch_processing(
        &self,
        epoch_snapshot: Arc<EpochSnapshot>,
        response_sender: Option<oneshot::Sender<eyre::Result<()>>>,
    ) {
        let clone = self.clone();
        std::thread::spawn(move || {
            // Mirror of `spawn_pruning_task`: panic survival so the
            // `epoch_running` flag is cleared and waiters resolve.
            let res: eyre::Result<()> =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    clone.on_epoch_processed(epoch_snapshot)
                })) {
                    Ok(r) => r,
                    Err(payload) => {
                        let msg = panic_payload_string(payload);
                        error!(custom.panic = %msg, "spawn_epoch_processing body panicked");
                        Err(eyre::eyre!("spawn_epoch_processing panicked: {msg}"))
                    }
                };
            let completion = match &res {
                Ok(_) => Ok(()),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            };
            if let Some(sender) = response_sender
                && let Err(e) = sender.send(res)
            {
                warn!(custom.error = ?e, "Unable to send a response for OnEpochProcessed")
            }
            // Notify service that epoch processing finished (drive the queue)
            if let Err(e) = clone
                .cache_sender
                .send_traced(CacheServiceAction::EpochProcessingCompleted(completion))
            {
                warn!(custom.error = ?e, "Failed to notify EpochProcessingCompleted");
            }
        });
    }
}

/// Extract a human-readable message from a `catch_unwind` payload.
/// `Box<dyn Any + Send>` panic payloads in practice carry either `&'static
/// str` or `String`; fall back to a generic label otherwise so the
/// resulting `eyre` chain is never empty.
fn panic_payload_string(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

pub type CacheServiceSender = UnboundedSender<Traced<CacheServiceAction>>;

#[derive(Debug)]
pub struct ChunkCacheService {
    pub msg_rx: UnboundedReceiver<Traced<CacheServiceAction>>,
    pub shutdown: Shutdown,
    pub cache_task: InnerCacheTask,
    // Serialized execution for each task type
    pruning_running: bool,
    pruning_queue: VecDeque<(u64, Option<oneshot::Sender<eyre::Result<()>>>)>,
    epoch_running: bool,
    epoch_queue: VecDeque<(
        Arc<EpochSnapshot>,
        Option<oneshot::Sender<eyre::Result<()>>>,
    )>,
    txid_prune_running: bool,
    txid_prune_queue: VecDeque<HashMap<H256, Vec<H256>>>,
}

impl ChunkCacheService {
    /// Spawns the chunk cache service on the provided runtime.
    ///
    /// The service manages cache eviction based on the configured strategy
    /// (time-based or size-based) and responds to block migration and epoch
    /// processing events.
    ///
    /// # Arguments
    ///
    /// * `block_index_guard` - Read guard for block index lookups
    /// * `block_tree_guard` - Read guard for canonical chain access
    /// * `db` - Database provider for cache storage
    /// * `rx` - Channel receiver for cache service actions
    /// * `config` - Node configuration including eviction strategy
    /// * `runtime_handle` - Tokio runtime for spawning the service
    ///
    /// # Returns
    ///
    /// A `TokioServiceHandle` for managing the service lifecycle
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_cache")]
    pub fn spawn_service(
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        db: DatabaseProvider,
        rx: UnboundedReceiver<Traced<CacheServiceAction>>,
        config: Config,
        gossip_broadcast: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
        cache_sender: CacheServiceSender,
        storage_modules_guard: StorageModulesReadGuard,
        ingress_proof_generation_state: IngressProofGenerationState,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning chunk cache service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(async move {
            let cache_service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                cache_task: InnerCacheTask {
                    db,
                    block_tree_guard,
                    block_index_guard,
                    config,
                    gossip_broadcast,
                    ingress_proof_generation_state,
                    cache_sender,
                    storage_modules_guard,
                    #[cfg(test)]
                    prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
                },
                pruning_running: false,
                pruning_queue: VecDeque::new(),
                epoch_running: false,
                epoch_queue: VecDeque::new(),
                txid_prune_running: false,
                txid_prune_queue: VecDeque::new(),
            };
            cache_service
                .start()
                .in_current_span()
                .await
                .expect("Chunk cache service encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "chunk_cache_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(name = "cache_service_start", level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting chunk cache service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for chunk cache service");
                    break;
                }
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, _entered) = traced.into_inner();
                            self.on_handle_message(msg);
                        }
                        None => {
                            warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, _entered) = traced.into_inner();
            self.on_handle_message(msg);
        }

        info!("shutting down chunk cache service gracefully");
        Ok(())
    }

    /// Dispatches incoming cache service messages to appropriate handlers.
    ///
    /// Handles two message types:
    /// - `OnBlockMigrated`: Triggers cache pruning based on block height
    /// - `OnEpochProcessed`: Triggers pruning based on epoch slot expiry
    fn on_handle_message(&mut self, msg: CacheServiceAction) {
        debug!(
            queue.pruning_running = ?self.pruning_running,
            queue.pruning_queued = ?self.pruning_queue.len(),
            queue.epoch_running = ?self.epoch_running,
            queue.epoch_queued = ?self.epoch_queue.len(),
            "CacheService received message: {:?}", msg
        );
        match msg {
            CacheServiceAction::OnBlockMigrated(migration_height, sender) => {
                // Enqueue pruning; start if idle
                self.pruning_queue.push_back((migration_height, sender));
                if !self.pruning_running
                    && let Some((h, s)) = self.pruning_queue.pop_front()
                {
                    self.pruning_running = true;
                    self.cache_task.spawn_pruning_task(h, s);
                }
            }
            CacheServiceAction::OnEpochProcessed(epoch_snapshot, sender) => {
                // Enqueue epoch processing; start if idle
                self.epoch_queue.push_back((epoch_snapshot, sender));
                if !self.epoch_running
                    && let Some((e, s)) = self.epoch_queue.pop_front()
                {
                    self.epoch_running = true;
                    self.cache_task.spawn_epoch_processing(e, s);
                }
            }
            CacheServiceAction::PruneCompleted(_res) => {
                // Mark pruning idle and kick next if queued
                self.pruning_running = false;
                if let Some((h, s)) = self.pruning_queue.pop_front() {
                    self.pruning_running = true;
                    self.cache_task.spawn_pruning_task(h, s);
                }
            }
            CacheServiceAction::EpochProcessingCompleted(_res) => {
                // Mark epoch processing idle and kick next if queued
                self.epoch_running = false;
                if let Some((e, s)) = self.epoch_queue.pop_front() {
                    self.epoch_running = true;
                    self.cache_task.spawn_epoch_processing(e, s);
                }
            }
            CacheServiceAction::PruneTxidsFromCachedDataRoots(by_data_root) => {
                // Enqueue txid pruning; start if idle
                self.txid_prune_queue.push_back(by_data_root);
                if !self.txid_prune_running
                    && let Some(work) = self.txid_prune_queue.pop_front()
                {
                    self.txid_prune_running = true;
                    self.cache_task.spawn_prune_txids_task(work);
                }
            }
            CacheServiceAction::PruneTxidsCompleted(res) => {
                // Mark txid pruning idle and kick next if queued
                self.txid_prune_running = false;
                // Surface batch failure so the silent drop is observable.
                // The worker already `warn!`s on Err (see
                // `spawn_prune_txids_task`); we only want a stable counter
                // here so dashboards can alert on it.  Stale tombstones
                // self-heal on the next scrub trigger for that data_root
                // (mempool TTL eviction or another failed reorg
                // re-ingress), so we don't requeue.
                if let Err(e) = res {
                    debug!(
                        error = %e,
                        "PruneTxidsCompleted reported worker Err — recording metric and continuing"
                    );
                    metrics::record_cache_txid_scrub_failed();
                }
                if let Some(work) = self.txid_prune_queue.pop_front() {
                    self.txid_prune_running = true;
                    self.cache_task.spawn_prune_txids_task(work);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use reth_db::transaction::DbTxMut as _;
    /// Cache tests exercise pruning policy, not storage placement; an empty
    /// module set means "nothing is durably stored locally".
    fn test_storage_modules_guard() -> StorageModulesReadGuard {
        StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new())))
    }

    /// A guard holding one storage module that genuinely reports `offsets` of
    /// `data_root` as fsynced `Data` — indexed, written, and flushed the way the
    /// real write path does. Needed because reclamation is gated on the storage
    /// module's own interval state, so an empty guard can never evict.
    fn storage_modules_guard_with_durable(
        base_directory: std::path::PathBuf,
        data_root: DataRoot,
        data_size: u64,
        offsets: &[u32],
    ) -> eyre::Result<StorageModulesReadGuard> {
        use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
        use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
        use irys_types::{
            ConsensusConfig, PartitionChunkOffset, RelativeChunkOffset,
            partition::PartitionAssignment, partition_chunk_offset_ie,
        };

        let num_chunks = 10_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: num_chunks,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory,
            ..NodeConfig::testing()
        };
        let sm_config = Config::new_with_random_peer_id(node_config);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
                miner_address: IrysAddress::random(),
                partition_hash: H256::random(),
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, num_chunks as u32),
                "chunks".into(),
            )],
        };
        let sm = Arc::new(StorageModule::new(&info, &sm_config)?);
        let (_, submodule) = sm.get_submodule_for_offset(PartitionChunkOffset::from(0_u32))?;
        submodule.db.update(|tx| {
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset::from(0_i32),
                    data_size,
                },
            )
        })?;
        for offset in offsets {
            sm.write_chunk(
                PartitionChunkOffset::from(*offset),
                vec![9_u8; 32],
                ChunkType::Data,
            );
        }
        sm.force_sync_pending_chunks()?;
        for offset in offsets {
            eyre::ensure!(
                sm.is_data_chunk_durable_at(PartitionChunkOffset::from(*offset)),
                "test fixture failed to make offset {offset} durable"
            );
        }
        Ok(StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            sm,
        ]))))
    }

    use super::*;
    use irys_database::DatabaseProviderTestExt as _;
    use irys_database::{
        database,
        db_cache::CachedDataRoot,
        tables::{CachedChunks, CachedChunksIndex, CachedDataRoots},
    };
    use irys_domain::{BlockIndex, BlockTree};
    use irys_testing_utils::{
        IrysBlockHeaderTestExt as _, initialize_tracing, new_mock_signed_header,
    };
    use irys_types::{
        Base64, Config, DataTransactionHeader, DataTransactionHeaderV1,
        DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, H256List, IrysAddress,
        NodeConfig, TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider,
    };
    use reth_db::cursor::DbDupCursorRO as _;
    use std::sync::{Arc, RwLock};

    /// `cache_data_root(_, _, None)` leaves expiry unset. Mempool ingress sets
    /// it, so the fixture writes `expiry_height` itself. `with_chunk` stores
    /// one body the way the unconfirmed-root test does.
    fn unconfirmed_cache(
        expiry_height: u64,
        with_chunk: bool,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        DatabaseProvider,
        irys_types::H256,
        ChunkCacheService,
    )> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(temp_dir.path(), &temp_dir.path().join("cache"))?;
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;
        db.update_cache(|wtx| {
            let mut cdr = wtx
                .get::<CachedDataRoots>(tx_header.data_root)?
                .ok_or_else(|| eyre::eyre!("missing CachedDataRoots entry"))?;
            cdr.expiry_height = Some(expiry_height);
            wtx.put::<CachedDataRoots>(tx_header.data_root, cdr)?;
            eyre::Ok(())
        })??;
        if with_chunk {
            let chunk = UnpackedChunk {
                data_root: tx_header.data_root,
                data_size: tx_header.data_size,
                data_path: Base64(vec![]),
                bytes: Base64(vec![0_u8; 8]),
                tx_offset: TxChunkOffset::from(0_u32),
            };
            db.update_cache(|wtx| {
                database::cache_chunk(wtx, &chunk)?;
                eyre::Ok(())
            })??;
        }
        let data_root = tx_header.data_root;
        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(has_root, "CachedDataRoots missing before prune");
            Ok(())
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let service = ChunkCacheService {
            msg_rx: rx,
            shutdown: shutdown_rx,
            cache_task: InnerCacheTask {
                db: db.clone(),
                block_tree_guard,
                block_index_guard,
                config,
                gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
                ingress_proof_generation_state: IngressProofGenerationState::new(),
                cache_sender: tx,
                storage_modules_guard: test_storage_modules_guard(),
                #[cfg(test)]
                prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
            },
            pruning_running: false,
            pruning_queue: VecDeque::new(),
            epoch_running: false,
            epoch_queue: VecDeque::new(),
            txid_prune_running: false,
            txid_prune_queue: VecDeque::new(),
        };
        Ok((temp_dir, db, data_root, service))
    }

    /// A mempool-only root (empty `block_set`, future expiry) must survive
    /// `prune_height > 0`.
    #[tokio::test]
    async fn does_not_prune_unconfirmed_data_roots() -> eyre::Result<()> {
        let (_temp_dir, db, data_root, service) = unconfirmed_cache(100, true)?;
        service.cache_task.prune_data_root_cache(1)?;
        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(has_root, "CachedDataRoots was prematurely pruned");
            Ok(())
        })??;
        Ok(())
    }

    /// An expired, never-confirmed root is pruned once `prune_height` passes
    /// its expiry.
    #[tokio::test]
    async fn prunes_expired_never_confirmed_data_root() -> eyre::Result<()> {
        let (_temp_dir, db, data_root, service) = unconfirmed_cache(5, false)?;
        service.cache_task.prune_data_root_cache(6)?;
        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(!has_root, "CachedDataRoots should have been pruned");
            Ok(())
        })??;
        Ok(())
    }

    /// What one capacity-prune fixture body looks like on this node.
    #[derive(Clone, Copy, Default)]
    struct OldBody {
        local_proof: bool,
        assigned: bool,
        migrated: bool,
        /// The storage module holds offset 0 as persisted `Data`.
        durable: bool,
        /// This signer recorded the chunk's compact ingress hash.
        ingress_hash: bool,
        /// The root's `CachedDataRoots` row is gone, as for a body ingress
        /// cached with its data size taken from a storage-module index.
        no_root_record: bool,
    }

    /// Runs one capacity prune over an old cached body. Reports whether it is
    /// still cached, and the pass tally.
    fn old_body_capacity_prune(body: OldBody) -> eyre::Result<(bool, PruneTally)> {
        let OldBody {
            local_proof,
            assigned,
            migrated,
            durable,
            ingress_hash,
            no_root_record,
        } = body;
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            db.update_cache_eyre(|cache_tx| {
                database::cache_data_root(cache_tx, &tx_header, None)?;
                if migrated {
                    irys_database::set_data_tx_included_height(wtx, &tx_header.id, 1)
                        .map_err(|e| eyre::eyre!("set_data_tx_included_height: {e:?}"))?;
                }
                eyre::Ok(())
            })
        })??;
        let chunk = UnpackedChunk {
            data_root: tx_header.data_root,
            data_size: tx_header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![2_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update_cache(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            // Mark the cached chunk as very old so it qualifies for age-based pruning
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(tx_header.data_root), None)?;
            // Put the aged entries back after the walk: MDBX aborts when this table gets a
            // write while the walk deletes through its cursor (see
            // `put_during_delete_current_walk_aborts` in irys-database).
            let mut aged_entries = Vec::new();
            while let Some((_k, entry)) = walk.next().transpose()? {
                walk.delete_current()?;
                let mut updated = entry;
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
                aged_entries.push(updated);
            }
            for updated in aged_entries {
                wtx.put::<CachedChunksIndex>(tx_header.data_root, updated)?;
            }
            eyre::Ok(())
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        if local_proof {
            db.update_cache(|wtx| {
                let mut ingress_proof = IngressProof::default();
                ingress_proof.data_root = tx_header.data_root;
                irys_database::store_external_ingress_proof_checked(
                    wtx,
                    &ingress_proof,
                    config.irys_signer().address(),
                )?;
                eyre::Ok(())
            })??;
        }
        if no_root_record {
            db.update_cache(|wtx| {
                wtx.delete::<CachedDataRoots>(tx_header.data_root, None)?;
                eyre::Ok(())
            })??;
        }
        if ingress_hash {
            db.update_cache(|wtx| {
                irys_database::store_ingress_data_hash(
                    wtx.inner(),
                    tx_header.data_root,
                    config.irys_signer().address(),
                    TxChunkOffset::from(0_u32),
                    H256::random(),
                )?;
                eyre::Ok(())
            })??;
        }
        let storage_modules_guard = if assigned || durable {
            // A placement for the chunk, persisted `Data` only when durable.
            let durable_offsets: &[u32] = if durable { &[0] } else { &[] };
            storage_modules_guard_with_durable(
                _temp_dir.path().join("sm"),
                tx_header.data_root,
                tx_header.data_size,
                durable_offsets,
            )?
        } else {
            test_storage_modules_guard()
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        let tally = service_task.prune_durably_migrated_chunk_bodies(10)?;
        let survives = db.view_cache(|rtx| -> eyre::Result<bool> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(tx_header.data_root))?;
            Ok(walk
                .next()
                .transpose()?
                .is_some_and(|(root, _)| root == tx_header.data_root))
        })??;
        Ok((survives, tally))
    }

    fn old_body_survives_capacity_prune(
        local_proof: bool,
        assigned: bool,
        migrated: bool,
    ) -> eyre::Result<bool> {
        old_body_capacity_prune(OldBody {
            local_proof,
            assigned,
            migrated,
            ..Default::default()
        })
        .map(|(survives, _)| survives)
    }

    /// Every body a pass collects lands in exactly one outcome.
    #[tokio::test]
    async fn the_prune_tally_names_why_each_body_was_kept() -> eyre::Result<()> {
        let migrated = OldBody {
            migrated: true,
            ..Default::default()
        };
        let cases = [
            (
                migrated,
                true,
                PruneTally {
                    deleted_unplaced: 1,
                    unplaced_attempted: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody::default(),
                false,
                PruneTally {
                    root_not_migrated: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    no_root_record: true,
                    ..migrated
                },
                false,
                PruneTally {
                    root_not_migrated: 1,
                    root_no_record: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    assigned: true,
                    ..OldBody::default()
                },
                false,
                PruneTally {
                    root_not_migrated: 1,
                    root_not_migrated_placed: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    local_proof: true,
                    ..migrated
                },
                false,
                PruneTally {
                    not_durable: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    assigned: true,
                    ..migrated
                },
                false,
                PruneTally {
                    not_durable: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    durable: true,
                    ..migrated
                },
                false,
                PruneTally {
                    no_ingress_hash: 1,
                    ..Default::default()
                },
            ),
            (
                OldBody {
                    durable: true,
                    ingress_hash: true,
                    ..migrated
                },
                true,
                PruneTally {
                    deleted_durable: 1,
                    ..Default::default()
                },
            ),
        ];
        for (index, (body, deleted, expected)) in cases.into_iter().enumerate() {
            let (survives, tally) = old_body_capacity_prune(body)?;
            assert_eq!(survives, !deleted, "case {index}");
            assert_eq!(
                tally,
                PruneTally {
                    candidates: 1,
                    ..expected
                },
                "case {index}"
            );
        }
        Ok(())
    }

    // Only bodies this node stores or proves wait for local durability.
    #[tokio::test]
    async fn capacity_prune_protects_only_stored_or_proven_bodies() -> eyre::Result<()> {
        assert!(
            !old_body_survives_capacity_prune(false, false, true)?,
            "a migrated body this node neither stores nor proves is reclaimed on age"
        );
        assert!(
            old_body_survives_capacity_prune(false, false, false)?,
            "before migration no node has a placement, so the body must stay"
        );
        assert!(
            old_body_survives_capacity_prune(true, false, true)?,
            "a body under a local ingress proof stays until durable"
        );
        assert!(
            old_body_survives_capacity_prune(false, true, true)?,
            "a body this node is assigned to store stays until durable"
        );
        assert!(old_body_survives_capacity_prune(true, true, true)?);
        Ok(())
    }

    /// At the hard cap, an expired local proof is the only fence for a cached
    /// body this node has not persisted. `prune_cache` must keep both.
    #[tokio::test]
    async fn capacity_prune_keeps_unpersisted_body_behind_expired_local_proof() -> eyre::Result<()>
    {
        let (proofs, chunks) = prune_cache_expired_proof(0, true, false, 1, None)?;
        assert_eq!(
            proofs, 1,
            "an expired local proof must stay while a cached body is unpersisted"
        );
        assert_eq!(chunks, 1, "the unpersisted body must stay with that proof");
        Ok(())
    }

    /// One persisted offset does not release the proof while another cached
    /// offset of the same root is still unpersisted.
    #[tokio::test]
    async fn capacity_prune_keeps_local_proof_while_any_cached_body_is_unpersisted()
    -> eyre::Result<()> {
        let (proofs, chunks) = prune_cache_expired_proof(0, true, false, 2, Some(&[0]))?;
        assert_eq!(
            proofs, 1,
            "an unpersisted sibling still needs the local proof"
        );
        assert_eq!(
            chunks, 2,
            "neither cached body is deleted with the proof in place"
        );
        Ok(())
    }

    /// Orders the cap must still allow: no cached body, every cached body
    /// persisted, a remote proof, a fully promoted local proof, and a cache
    /// that is under the hard cap.
    #[tokio::test]
    async fn capacity_prune_still_drops_proofs_that_do_not_fence_unpersisted_bodies()
    -> eyre::Result<()> {
        let (proofs, chunks) = prune_cache_expired_proof(0, true, false, 0, None)?;
        assert_eq!(
            proofs, 0,
            "no cached body: the expired local proof is deleted"
        );
        assert_eq!(chunks, 0);

        let (proofs, chunks) = prune_cache_expired_proof(0, true, false, 1, Some(&[0]))?;
        assert_eq!(
            proofs, 0,
            "persisted Data: the expired local proof is deleted"
        );
        assert_eq!(
            chunks, 1,
            "the durable reclaim still needs the ingress hash"
        );

        let (proofs, chunks) = prune_cache_expired_proof(0, false, false, 1, None)?;
        assert_eq!(proofs, 0, "a remote proof is not a local fence");
        assert_eq!(
            chunks, 0,
            "the migrated body this node does not store is reclaimed"
        );

        let (proofs, chunks) = prune_cache_expired_proof(0, true, true, 1, None)?;
        assert_eq!(
            proofs, 0,
            "a fully promoted local proof still expires at capacity"
        );
        assert_eq!(chunks, 0, "promotion removes the unpromoted fence");

        let (proofs, chunks) = prune_cache_expired_proof(10_000, true, false, 1, None)?;
        assert_eq!(
            proofs, 1,
            "under the hard cap the local proof is not deleted"
        );
        assert_eq!(
            chunks, 1,
            "under the prune threshold the body is not scanned"
        );
        Ok(())
    }

    /// One `prune_cache` over an expired proof and `chunk_count` old migrated
    /// bodies. Returns `(proof rows left, cached chunk index rows left)`.
    ///
    /// `persisted_offsets` of `Some` installs one module that reports those
    /// partition offsets as persisted `Data`. `None` installs no module.
    fn prune_cache_expired_proof(
        max_cache_size_bytes: u64,
        local_proof: bool,
        promoted: bool,
        chunk_count: u32,
        persisted_offsets: Option<&[u32]>,
    ) -> eyre::Result<(usize, usize)> {
        let mut node_config = NodeConfig::testing();
        node_config.cache.max_cache_size_bytes = max_cache_size_bytes;
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;
        let data_size = 32_u64.saturating_mul(u64::from(chunk_count.max(1)));
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let data_root = tx_header.data_root;
        db.update(|wtx| {
            db.update_cache_eyre(|cache_tx| {
                database::cache_data_root(cache_tx, &tx_header, None)?;
                database::insert_tx_header(wtx, &tx_header)?;
                irys_database::set_data_tx_included_height(wtx, &tx_header.id, 1)
                    .map_err(|e| eyre::eyre!("set_data_tx_included_height: {e:?}"))?;
                if promoted {
                    irys_database::set_data_tx_promoted_height(wtx, &tx_header.id, 2)
                        .map_err(|e| eyre::eyre!("set_data_tx_promoted_height: {e:?}"))?;
                }
                for offset in 0..chunk_count {
                    let chunk = UnpackedChunk {
                        data_root,
                        data_size,
                        data_path: Base64(vec![offset as u8]),
                        bytes: Base64(vec![2_u8; 8]),
                        tx_offset: TxChunkOffset::from(offset),
                    };
                    database::cache_chunk(cache_tx, &chunk)?;
                }
                if chunk_count > 0 {
                    let mut cur = cache_tx.cursor_dup_write::<CachedChunksIndex>()?;
                    let mut walk = cur.walk_dup(Some(data_root), None)?;
                    // Put the aged entries back after the walk: MDBX aborts when this table gets a
                    // write while the walk deletes through its cursor (see
                    // `put_during_delete_current_walk_aborts` in irys-database).
                    let mut aged_entries = Vec::new();
                    while let Some((_k, entry)) = walk.next().transpose()? {
                        walk.delete_current()?;
                        let mut updated = entry;
                        updated.meta.updated_at = UnixTimestamp::from_secs(0);
                        aged_entries.push(updated);
                    }
                    for updated in aged_entries {
                        cache_tx.put::<CachedChunksIndex>(data_root, updated)?;
                    }
                }
                let mut proof = IngressProof::default();
                proof.data_root = data_root;
                proof.anchor = H256::random();
                let address = if local_proof {
                    config.irys_signer().address()
                } else {
                    IrysAddress::random()
                };
                irys_database::store_external_ingress_proof_checked(cache_tx, &proof, address)?;
                eyre::Ok(())
            })
        })??;

        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let storage_modules_guard = match persisted_offsets {
            Some(offsets) => storage_modules_guard_with_durable(
                sm_dir.path().to_path_buf(),
                data_root,
                data_size,
                offsets,
            )?,
            None => test_storage_modules_guard(),
        };
        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        service_task.prune_cache(10)?;

        let proofs = db.view_cache_eyre(|rtx| {
            Ok(irys_database::ingress_proofs_by_data_root(rtx, data_root)?.len())
        })?;
        let chunks = db.view_cache_eyre(|rtx| {
            let mut cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let walker = cursor.walk_dup(Some(data_root), None)?;
            let rows = walker.collect::<Result<Vec<_>, _>>()?;
            Ok(rows.len())
        })?;
        Ok((proofs, chunks))
    }

    /// After open drops a placement, capacity prune must keep the replica's
    /// cached body while the assigned interval is not persisted `Data`.
    ///
    /// The ledger offset is slot 0. A module on another slot is not the
    /// custodian. Persisted `Data` ends the crash window, so that copy can go.
    #[tokio::test]
    async fn capacity_prune_keeps_replica_cache_after_placement_drop() -> eyre::Result<()> {
        assert!(
            replica_body_survives_capacity_prune(0, false)?,
            "assigned slot with an unpersisted interval must keep the cache after the placement row is gone"
        );
        assert!(
            !replica_body_survives_capacity_prune(1, false)?,
            "a module assigned a different slot does not keep this chunk"
        );
        assert!(
            !replica_body_survives_capacity_prune(0, true)?,
            "persisted Data ends the crash window; the cache copy can go"
        );
        Ok(())
    }

    /// One old migrated body, no local proof, no placement row.
    ///
    /// `persist_data` writes partition offset 0 as `Data`, then deletes the
    /// data-root row so the index looks like the open-time drop while the
    /// file still says `Data`.
    fn replica_body_survives_capacity_prune(
        slot_index: usize,
        persist_data: bool,
    ) -> eyre::Result<bool> {
        use irys_database::insert_tx_header;
        use irys_database::tables::{IrysBlockHeaders, MigratedBlockHashes};
        use irys_types::{H256List, IrysBlockHeader, PartitionChunkOffset, U256};

        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let chunk_size = config.consensus.chunk_size;
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: chunk_size,
                ledger_id: DataLedger::Submit as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let data_root = tx_header.data_root;
        db.update(|wtx| {
            db.update_cache_eyre(|cache_tx| {
                database::cache_data_root(cache_tx, &tx_header, None)?;
                insert_tx_header(wtx, &tx_header)?;
                irys_database::set_data_tx_included_height(wtx, &tx_header.id, 1)
                    .map_err(|e| eyre::eyre!("set_data_tx_included_height: {e:?}"))?;
                eyre::Ok(())
            })
        })??;
        let chunk = UnpackedChunk {
            data_root,
            data_size: chunk_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![2_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update_cache(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(data_root), None)?;
            // Put the aged entries back after the walk: MDBX aborts when this table gets a
            // write while the walk deletes through its cursor (see
            // `put_during_delete_current_walk_aborts` in irys-database).
            let mut aged_entries = Vec::new();
            while let Some((_k, entry)) = walk.next().transpose()? {
                walk.delete_current()?;
                let mut updated = entry;
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
                aged_entries.push(updated);
            }
            for updated in aged_entries {
                wtx.put::<CachedChunksIndex>(data_root, updated)?;
            }
            eyre::Ok(())
        })??;

        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = H256::zero();
        genesis.data_ledgers[DataLedger::Submit].total_chunks = 0;
        genesis.test_sign();
        let mut included = IrysBlockHeader::new_mock_header();
        included.height = 1;
        included.previous_block_hash = genesis.block_hash;
        included.cumulative_diff = U256::from(2_u64);
        let submit = &mut included.data_ledgers[DataLedger::Submit];
        submit.total_chunks = 1;
        submit.tx_ids = H256List(vec![tx_header.id]);
        included.test_sign();
        db.update(|wtx| {
            wtx.put::<IrysBlockHeaders>(genesis.block_hash, genesis.clone().into())?;
            wtx.put::<MigratedBlockHashes>(0, genesis.block_hash)?;
            wtx.put::<IrysBlockHeaders>(included.block_hash, included.clone().into())?;
            wtx.put::<MigratedBlockHashes>(1, included.block_hash)?;
            eyre::Ok(())
        })??;

        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let storage_modules_guard = if persist_data {
            let guard = storage_modules_guard_with_durable(
                sm_dir.path().to_path_buf(),
                data_root,
                chunk_size,
                &[0],
            )?;
            let sm = guard.read()[0].clone();
            sm.clear_data_root_infos_in_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(0),
                &[data_root],
            )?;
            eyre::ensure!(
                sm.is_data_chunk_persisted_at(PartitionChunkOffset::from(0)),
                "fixture did not persist Data"
            );
            guard
        } else {
            assigned_module_without_placement(sm_dir.path().to_path_buf(), slot_index)?
        };
        {
            let sm = storage_modules_guard.read()[0].clone();
            let placed = sm.placed_tx_offsets_for_data_root(
                data_root,
                &BTreeSet::from([TxChunkOffset::from(0)]),
            )?;
            eyre::ensure!(placed.is_empty(), "fixture still has a placement row");
            if !persist_data {
                eyre::ensure!(
                    !sm.is_data_chunk_persisted_at(PartitionChunkOffset::from(0)),
                    "unpersisted fixture is already Data"
                );
            }
        }

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        service_task.prune_durably_migrated_chunk_bodies(10)?;
        db.view_cache(|rtx| -> eyre::Result<bool> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(data_root))?;
            Ok(walk
                .next()
                .transpose()?
                .is_some_and(|(root, _)| root == data_root))
        })?
    }

    /// Ledger-assigned module with an empty index. Intervals stay `Uninitialized`.
    fn assigned_module_without_placement(
        base_directory: std::path::PathBuf,
        slot_index: usize,
    ) -> eyre::Result<StorageModulesReadGuard> {
        use irys_domain::{StorageModule, StorageModuleInfo};
        use irys_types::{
            ConsensusConfig, PartitionChunkOffset, partition::PartitionAssignment,
            partition_chunk_offset_ie,
        };

        let num_chunks = 10_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: num_chunks,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory,
            ..NodeConfig::testing()
        };
        let sm_config = Config::new_with_random_peer_id(node_config);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(slot_index),
                miner_address: IrysAddress::random(),
                partition_hash: H256::random(),
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, num_chunks as u32),
                "chunks".into(),
            )],
        };
        let sm = Arc::new(StorageModule::new(&info, &sm_config)?);
        Ok(StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            sm,
        ]))))
    }

    /// One assigned span. `persisted_own` writes this root as `Data` at partition 0.
    ///
    /// The fixture partition is 10 chunks. Returns `(kept, index span views, interval visits)`
    /// for the single candidate.
    fn span_keep(case: SpanKeepCase) -> eyre::Result<(bool, u64, u64)> {
        use irys_database::insert_tx_header;
        use irys_database::tables::{IrysBlockHeaders, MigratedBlockHashes};
        use irys_types::{H256, IrysBlockHeader, U256};

        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let chunk_size = config.consensus.chunk_size;
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        let cached = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                data_root: H256::random(),
                data_size: case.tx_chunks * chunk_size,
                ledger_id: DataLedger::Submit as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let data_root = cached.data_root;
        let mut tx_ids = Vec::with_capacity(case.prior_chunks.len() + 1);
        db.update(|wtx| {
            db.update_cache_eyre(|cache_tx| {
                for chunks in case.prior_chunks {
                    let prior = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                        tx: DataTransactionHeaderV1 {
                            id: H256::random(),
                            data_root: H256::random(),
                            data_size: chunks * chunk_size,
                            ledger_id: DataLedger::Submit.into(),
                            ..Default::default()
                        },
                        metadata: DataTransactionMetadata::new(),
                    });
                    insert_tx_header(wtx, &prior)?;
                    tx_ids.push(prior.id);
                }
                database::cache_data_root(cache_tx, &cached, None)?;
                insert_tx_header(wtx, &cached)?;
                irys_database::set_data_tx_included_height(wtx, &cached.id, 1)
                    .map_err(|e| eyre::eyre!("set_data_tx_included_height: {e:?}"))?;
                eyre::Ok(())
            })
        })??;
        tx_ids.push(cached.id);

        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = H256::zero();
        genesis.data_ledgers[DataLedger::Submit].total_chunks = case.prev_total_chunks;
        genesis.test_sign();
        let mut included = IrysBlockHeader::new_mock_header();
        included.height = 1;
        included.previous_block_hash = genesis.block_hash;
        included.cumulative_diff = U256::from(2_u64);
        let submit = &mut included.data_ledgers[DataLedger::Submit];
        submit.total_chunks = case.prev_total_chunks;
        submit.tx_ids = H256List(tx_ids);
        included.test_sign();
        db.update(|wtx| {
            wtx.put::<IrysBlockHeaders>(genesis.block_hash, genesis.clone().into())?;
            wtx.put::<MigratedBlockHashes>(0, genesis.block_hash)?;
            wtx.put::<IrysBlockHeaders>(included.block_hash, included.clone().into())?;
            wtx.put::<MigratedBlockHashes>(1, included.block_hash)?;
            eyre::Ok(())
        })??;

        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let storage_modules_guard = if case.persisted_own {
            storage_modules_guard_with_durable(
                sm_dir.path().to_path_buf(),
                data_root,
                chunk_size,
                &[0],
            )?
        } else {
            assigned_module_without_placement(sm_dir.path().to_path_buf(), case.slot_index)?
        };
        let sm = storage_modules_guard.read()[0].clone();
        eyre::ensure!(
            sm.config.consensus.num_chunks_in_partition == 10,
            "span_keep assumes a 10-chunk partition"
        );

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = InnerCacheTask {
            db,
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        let start = task
            .ledger_chunk_start(&included, DataLedger::Submit)?
            .expect("previous block names the ledger start");
        assert_eq!(*start, case.prev_total_chunks);
        let candidate = TxChunkOffset::from(case.candidate);
        let views_before = sm.index_view_count();
        let visits_before = sm.interval_visit_count();
        let keep = task.assigned_unpersisted_offsets(
            data_root,
            &BTreeSet::from([candidate]),
            &task.storage_modules_guard.read(),
            10,
        )?;
        let views = sm.index_view_count().saturating_sub(views_before);
        let visits = sm.interval_visit_count().saturating_sub(visits_before);
        Ok((keep.contains(&candidate), views, visits))
    }

    /// A fitting assigned chunk is kept. An offset that does not fit is skipped.
    ///
    /// One candidate is one point read: a span index view or an interval walk
    /// would count above zero.
    #[tokio::test]
    async fn ledger_span_keeps_a_fitting_offset_and_skips_one_that_does_not_fit() -> eyre::Result<()>
    {
        let (kept, views, visits) = span_keep(SpanKeepCase {
            prev_total_chunks: 0,
            prior_chunks: &[],
            tx_chunks: 1,
            candidate: 0,
            slot_index: 0,
            persisted_own: false,
        })?;
        assert!(kept, "an assigned unpersisted chunk stays in the cache");
        assert_eq!(views, 0, "one candidate must not open a span index view");
        assert_eq!(visits, 0, "one candidate must not walk the interval map");

        let (kept, views, visits) = span_keep(SpanKeepCase {
            prev_total_chunks: 10,
            prior_chunks: &[],
            tx_chunks: 1,
            candidate: 0,
            slot_index: 1,
            persisted_own: false,
        })?;
        assert!(
            kept,
            "a chunk at the start of slot 1 is still kept while it is unpersisted"
        );
        assert_eq!(views, 0, "one candidate must not open a span index view");
        assert_eq!(visits, 0, "one candidate must not walk the interval map");

        let (kept, _, _) = span_keep(SpanKeepCase {
            prev_total_chunks: 0,
            prior_chunks: &[],
            tx_chunks: 1,
            candidate: 0,
            slot_index: 0,
            persisted_own: true,
        })?;
        assert!(
            !kept,
            "persisted Data for this chunk still releases the cache"
        );

        let (kept, _, _) = span_keep(SpanKeepCase {
            prev_total_chunks: 0,
            prior_chunks: &[],
            tx_chunks: 1,
            candidate: 5,
            slot_index: 0,
            persisted_own: false,
        })?;
        assert!(
            !kept,
            "a candidate past the transaction is not part of the span"
        );

        // Tx offset 1 sits one past u64::MAX. Wrapping that sum lands on slot 0.
        let (kept, _, _) = span_keep(SpanKeepCase {
            prev_total_chunks: u64::MAX,
            prior_chunks: &[],
            tx_chunks: 2,
            candidate: 1,
            slot_index: 0,
            persisted_own: false,
        })?;
        assert!(!kept, "a ledger offset that does not fit is skipped");

        // The prior transaction's 2 chunks push the cursor past u64::MAX.
        // Wrapping would place this transaction at ledger offset 0.
        let (kept, _, _) = span_keep(SpanKeepCase {
            prev_total_chunks: u64::MAX - 1,
            prior_chunks: &[2],
            tx_chunks: 1,
            candidate: 0,
            slot_index: 0,
            persisted_own: false,
        })?;
        assert!(
            !kept,
            "a cursor that does not fit does not invent a span at offset 0"
        );

        // 10 chunks per partition. This slot product wraps to ledger offset 4.
        let slot = usize::try_from(u64::MAX / 10 + 1).expect("slot index fits usize");
        let (kept, views, visits) = span_keep(SpanKeepCase {
            prev_total_chunks: 4,
            prior_chunks: &[],
            tx_chunks: 1,
            candidate: 0,
            slot_index: slot,
            persisted_own: false,
        })?;
        assert!(!kept, "a slot product that does not fit is skipped");
        assert_eq!(views, 0, "an unfit slot must not open an index view");
        assert_eq!(visits, 0, "an unfit slot must not walk the interval map");
        Ok(())
    }

    struct SpanKeepCase {
        prev_total_chunks: u64,
        prior_chunks: &'static [u64],
        tx_chunks: u64,
        candidate: u32,
        slot_index: usize,
        persisted_own: bool,
    }

    /// The slot that holds the cached body moved onto a partition whose file
    /// still says `Data` for a previous root. Capacity prune must keep that
    /// body. A `Data` label with no index row, a placement of this root, and
    /// a module on another slot still release the body.
    #[tokio::test]
    async fn capacity_prune_keeps_body_when_data_label_names_another_root() -> eyre::Result<()> {
        let (survived, views, visits) = shifted_slot_body_survives(SlotLabel::ForeignData)?;
        assert!(
            survived,
            "a Data label that names another root is not this chunk; the cache stays"
        );
        assert_eq!(views, 0, "one candidate must not open a span index view");
        assert_eq!(visits, 0, "one candidate must not walk the interval map");
        assert!(
            shifted_slot_body_survives(SlotLabel::ForeignUnpersisted)?.0,
            "an assigned span that is not Data still keeps the cache"
        );
        assert!(
            !shifted_slot_body_survives(SlotLabel::ClearedData)?.0,
            "persisted Data with no index row still ends the crash window"
        );
        assert!(
            !shifted_slot_body_survives(SlotLabel::OwnData)?.0,
            "a placement of this root and persisted Data still delete the cache"
        );
        assert!(
            !shifted_slot_body_survives(SlotLabel::OtherSlot)?.0,
            "a module on another slot does not keep this chunk"
        );
        Ok(())
    }

    /// What the partition index says about the offset the new slot maps onto.
    #[derive(Clone, Copy)]
    enum SlotLabel {
        /// Full placement of another root, and the file says `Data`.
        ForeignData,
        /// Full placement of another root, interval still uninitialized.
        ForeignUnpersisted,
        /// This root was written as `Data`, then its placement row was removed.
        ClearedData,
        /// This root's placement row is present and the file says `Data`.
        OwnData,
        /// The module's slot does not cover the cached span.
        OtherSlot,
    }

    /// One old migrated body at the start of slot 1. `num_chunks` is 10, so
    /// the span's ledger offset is 10 and slot 1 maps it to partition offset 0.
    ///
    /// Returns `(body still cached, index span views during prune, interval
    /// visits during prune)`.
    fn shifted_slot_body_survives(label: SlotLabel) -> eyre::Result<(bool, u64, u64)> {
        use irys_database::insert_tx_header;
        use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
        use irys_database::tables::{IrysBlockHeaders, MigratedBlockHashes};
        use irys_domain::ChunkType;
        use irys_types::{
            DataTransactionLedger, H256, IrysBlockHeader, LedgerChunkOffset, LedgerChunkRange,
            PartitionChunkOffset, RelativeChunkOffset, U256, irys::IrysSigner,
        };
        use nodit::interval::ii;

        const NUM_CHUNKS: u64 = 10;

        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let chunk_size = config.consensus.chunk_size;
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: chunk_size,
                ledger_id: DataLedger::Submit as u32,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let data_root = tx_header.data_root;
        let local_address = config.irys_signer().address();
        db.update(|wtx| {
            db.update_cache_eyre(|cache_tx| {
                database::cache_data_root(cache_tx, &tx_header, None)?;
                insert_tx_header(wtx, &tx_header)?;
                irys_database::set_data_tx_included_height(wtx, &tx_header.id, 1)
                    .map_err(|e| eyre::eyre!("set_data_tx_included_height: {e:?}"))?;
                eyre::Ok(())
            })
        })??;
        let chunk = UnpackedChunk {
            data_root,
            data_size: chunk_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![2_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update_cache(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(data_root), None)?;
            // Put the aged entries back after the walk: MDBX aborts when this table gets a
            // write while the walk deletes through its cursor (see
            // `put_during_delete_current_walk_aborts` in irys-database).
            let mut aged_entries = Vec::new();
            while let Some((_k, entry)) = walk.next().transpose()? {
                walk.delete_current()?;
                let mut updated = entry;
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
                aged_entries.push(updated);
            }
            for updated in aged_entries {
                wtx.put::<CachedChunksIndex>(data_root, updated)?;
            }
            if matches!(label, SlotLabel::OwnData) {
                irys_database::store_ingress_data_hash(
                    wtx.inner(),
                    data_root,
                    local_address,
                    chunk.tx_offset,
                    irys_types::generate_ingress_data_hash(&chunk.bytes.0, local_address),
                )?;
            }
            eyre::Ok(())
        })??;

        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = H256::zero();
        genesis.data_ledgers[DataLedger::Submit].total_chunks = NUM_CHUNKS;
        genesis.test_sign();
        let mut included = IrysBlockHeader::new_mock_header();
        included.height = 1;
        included.previous_block_hash = genesis.block_hash;
        included.cumulative_diff = U256::from(2_u64);
        let submit = &mut included.data_ledgers[DataLedger::Submit];
        submit.total_chunks = NUM_CHUNKS + 1;
        submit.tx_ids = H256List(vec![tx_header.id]);
        included.test_sign();
        db.update(|wtx| {
            wtx.put::<IrysBlockHeaders>(genesis.block_hash, genesis.clone().into())?;
            wtx.put::<MigratedBlockHashes>(0, genesis.block_hash)?;
            wtx.put::<IrysBlockHeaders>(included.block_hash, included.clone().into())?;
            wtx.put::<MigratedBlockHashes>(1, included.block_hash)?;
            eyre::Ok(())
        })??;

        let module_slot = match label {
            SlotLabel::OtherSlot => 0,
            _ => 1,
        };
        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let storage_modules_guard =
            assigned_module_without_placement(sm_dir.path().to_path_buf(), module_slot)?;
        let sm = storage_modules_guard.read()[0].clone();
        let partition_zero = PartitionChunkOffset::from(0_u32);
        match label {
            SlotLabel::ForeignData | SlotLabel::ForeignUnpersisted | SlotLabel::OtherSlot => {
                let ledger_at = if matches!(label, SlotLabel::OtherSlot) {
                    0
                } else {
                    NUM_CHUNKS
                };
                let signer = IrysSigner::random_signer(&sm.config.consensus);
                let bytes = vec![4_u8; sm.config.consensus.chunk_size as usize];
                let old_tx = signer
                    .create_transaction(bytes, H256::zero())
                    .expect("old tx");
                let old_tx = signer.sign_transaction(old_tx).expect("sign old tx");
                eyre::ensure!(
                    old_tx.header.data_root != data_root,
                    "fixture old root collided with the cached root"
                );
                let (_tx_root, proofs) =
                    DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&old_tx.header));
                sm.index_transaction_data(
                    &old_tx.header,
                    &proofs[0].proof,
                    LedgerChunkRange(ii(
                        LedgerChunkOffset::from(ledger_at),
                        LedgerChunkOffset::from(ledger_at),
                    )),
                    0,
                )?;
                let named = sm
                    .data_root_and_tx_offset_at(partition_zero)?
                    .map(|(root, _)| root);
                eyre::ensure!(
                    named == Some(old_tx.header.data_root),
                    "fixture index does not name the old root at partition offset 0, got {named:?}"
                );
            }
            SlotLabel::ClearedData | SlotLabel::OwnData => {
                let (_, submodule) = sm.get_submodule_for_offset(partition_zero)?;
                submodule.db.update(|tx| {
                    tx.add_data_root_info(
                        data_root,
                        &DataRootInfo {
                            start_offset: RelativeChunkOffset::from(0_i32),
                            data_size: sm.config.consensus.chunk_size,
                        },
                    )
                })?;
            }
        }
        let write_data = !matches!(label, SlotLabel::ForeignUnpersisted);
        if write_data {
            let offsets: &[u32] = if matches!(label, SlotLabel::ForeignData) {
                &[0, 2, 4]
            } else {
                &[0]
            };
            for offset in offsets {
                eyre::ensure!(
                    sm.write_chunk(
                        PartitionChunkOffset::from(*offset),
                        vec![9_u8; sm.config.consensus.chunk_size as usize],
                        ChunkType::Data,
                    ),
                    "fixture failed to queue offset {offset}"
                );
            }
            sm.force_sync_pending_chunks()?;
            eyre::ensure!(
                sm.is_data_chunk_persisted_at(partition_zero),
                "fixture offset 0 is not persisted Data"
            );
        } else {
            eyre::ensure!(
                !sm.is_data_chunk_persisted_at(partition_zero),
                "unpersisted fixture is already Data"
            );
        }
        if matches!(label, SlotLabel::ForeignData) {
            eyre::ensure!(
                sm.get_chunk_type(&PartitionChunkOffset::from(1_u32)) != Some(ChunkType::Data),
                "offsets 0 and 2 collapsed into one Data run"
            );
        }
        if matches!(label, SlotLabel::ClearedData) {
            sm.clear_data_root_infos_in_range(partition_zero, partition_zero, &[data_root])?;
            eyre::ensure!(
                sm.is_data_chunk_persisted_at(partition_zero),
                "clearing the placement row cleared the Data label"
            );
        }
        let placed = sm.placed_tx_offsets_for_data_root(
            data_root,
            &BTreeSet::from([TxChunkOffset::from(0)]),
        )?;
        match label {
            SlotLabel::OwnData => {
                eyre::ensure!(!placed.is_empty(), "own placement row is missing");
            }
            _ => {
                eyre::ensure!(placed.is_empty(), "fixture still places the cached root");
            }
        }

        let views_before = sm.index_view_count();
        let visits_before = sm.interval_visit_count();
        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        service_task.prune_durably_migrated_chunk_bodies(10)?;
        let views = sm.index_view_count().saturating_sub(views_before);
        let visits = sm.interval_visit_count().saturating_sub(visits_before);
        let survived = db.view_cache(|rtx| -> eyre::Result<bool> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(data_root))?;
            Ok(walk
                .next()
                .transpose()?
                .is_some_and(|(root, _)| root == data_root))
        })??;
        Ok((survived, views, visits))
    }

    /// A body that was durable in the snapshot stays in the cache when that
    /// offset is Uninitialized before the delete.
    #[tokio::test]
    async fn keeps_cached_body_when_offset_is_uninitialized_before_delete() -> eyre::Result<()> {
        use irys_domain::{ChunkType, StorageModule};
        use irys_types::PartitionChunkOffset;

        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let local_address = config.irys_signer().address();
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;
        let data_root = H256::random();
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 32,
                data_root,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let chunk = UnpackedChunk {
            data_root,
            data_size: 32,
            data_path: Base64(vec![1]),
            bytes: Base64(vec![7_u8; 32]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            database::cache_chunk(wtx, &chunk)?;
            irys_database::store_ingress_data_hash(
                wtx.inner(),
                data_root,
                local_address,
                chunk.tx_offset,
                irys_types::generate_ingress_data_hash(&chunk.bytes.0, local_address),
            )?;
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(data_root), None)?;
            // Put the aged entries back after the walk: MDBX aborts when this table gets a
            // write while the walk deletes through its cursor (see
            // `put_during_delete_current_walk_aborts` in irys-database).
            let mut aged_entries = Vec::new();
            while let Some((_k, entry)) = walk.next().transpose()? {
                walk.delete_current()?;
                let mut updated = entry;
                updated.meta.updated_at = irys_types::UnixTimestamp::from_secs(0);
                aged_entries.push(updated);
            }
            for updated in aged_entries {
                wtx.put::<CachedChunksIndex>(data_root, updated)?;
            }
            eyre::Ok(())
        })??;

        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let storage_modules_guard =
            storage_modules_guard_with_durable(sm_dir.path().to_path_buf(), data_root, 32, &[0])?;
        let sm: Arc<StorageModule> = storage_modules_guard.read()[0].clone();
        let offset = PartitionChunkOffset::from(0_u32);
        eyre::ensure!(
            sm.is_data_chunk_persisted_at(offset),
            "fixture offset is not persisted"
        );

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard,
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        task.arm_prune_recheck_pause();
        struct Release<'a>(&'a InnerCacheTask);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.release_prune_recheck_pause();
            }
        }
        let _release = Release(&task);
        let worker = task.clone();
        let handle =
            std::thread::spawn(move || worker.prune_durably_migrated_chunk_bodies(u64::MAX));
        eyre::ensure!(
            task.wait_for_prune_recheck_pause(std::time::Duration::from_secs(5)),
            "prune did not reach the durability re-check"
        );
        sm.reset()?;
        eyre::ensure!(
            sm.get_chunk_type(&offset) == Some(ChunkType::Uninitialized),
            "reset did not clear the offset"
        );
        eyre::ensure!(
            !sm.is_data_chunk_persisted_at(offset),
            "offset is still persisted after reset"
        );
        task.release_prune_recheck_pause();
        handle.join().expect("prune thread").expect("prune");

        db.view_cache(|rtx| -> eyre::Result<()> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(data_root))?;
            eyre::ensure!(
                walk.next().transpose()?.is_some(),
                "cached body was deleted after the offset became Uninitialized"
            );
            Ok(())
        })??;
        Ok(())
    }

    // Chunks older than threshold should be deleted while newer ones should remain.
    #[tokio::test]
    async fn prunes_only_migrated_chunks_older_than_threshold() -> eyre::Result<()> {
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // Create two data roots: one "old" and one "new"
        let tx_header_old = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                data_root: DataRoot::random(),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let tx_header_new = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                data_root: DataRoot::random(),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let address = IrysAddress::random();
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header_old, None)?;
            database::cache_data_root(wtx, &tx_header_new, None)?;
            eyre::Ok(())
        })??;

        let mk_chunk = |data_root: irys_types::H256, offset: u32, byte: u8| UnpackedChunk {
            data_root,
            data_size: 64,
            data_path: Base64(vec![]),
            bytes: Base64(vec![byte; 8]),
            tx_offset: TxChunkOffset::from(offset),
        };

        db.update_cache(|wtx| {
            // Manually insert index and chunk entries with controlled timestamps
            let chunk_old = mk_chunk(tx_header_old.data_root, 0, 10);
            let hash_old = chunk_old.chunk_path_hash();
            let idx_old = irys_database::db_cache::CachedChunkIndexEntry {
                index: chunk_old.tx_offset,
                meta: irys_database::db_cache::CachedChunkIndexMetadata {
                    chunk_path_hash: hash_old,
                    updated_at: UnixTimestamp::from_secs(0),
                },
            };
            wtx.put::<CachedChunksIndex>(tx_header_old.data_root, idx_old)?;
            wtx.put::<CachedChunks>(
                hash_old,
                irys_database::db_cache::CachedChunk::from(&chunk_old),
            )?;
            irys_database::store_ingress_data_hash(
                wtx.inner(),
                tx_header_old.data_root,
                address,
                chunk_old.tx_offset,
                irys_types::generate_ingress_data_hash(&chunk_old.bytes.0, address),
            )?;
            let chunk_new = mk_chunk(tx_header_new.data_root, 0, 11);
            let hash_new = chunk_new.chunk_path_hash();
            let idx_new = irys_database::db_cache::CachedChunkIndexEntry {
                index: chunk_new.tx_offset,
                meta: irys_database::db_cache::CachedChunkIndexMetadata {
                    chunk_path_hash: hash_new,
                    updated_at: UnixTimestamp::from_secs(10),
                },
            };
            wtx.put::<CachedChunksIndex>(tx_header_new.data_root, idx_new)?;
            wtx.put::<CachedChunks>(
                hash_new,
                irys_database::db_cache::CachedChunk::from(&chunk_new),
            )?;

            eyre::Ok(())
        })??;

        // Directly exercise the DB helper with a controlled cutoff time.
        // Use cutoff=5s so 0s is pruned and 10s is retained.
        db.view_cache(|rtx| -> eyre::Result<()> {
            let mut cur_old = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk_old = cur_old.walk_dup(Some(tx_header_old.data_root), None)?;
            let mut eligible = 0;
            while let Some((_k, entry)) = walk_old.next().transpose()? {
                if entry.meta.updated_at < UnixTimestamp::from_secs(5) {
                    eligible += 1;
                }
            }
            eyre::ensure!(
                eligible == 1,
                "Expected exactly one eligible old entry, found {}",
                eligible
            );
            Ok(())
        })??;

        db.update_cache(|wtx| {
            let pruned = irys_database::delete_reclaimable_cached_chunks_older_than(
                wtx,
                tx_header_old.data_root,
                address,
                &BTreeSet::from([TxChunkOffset::from(0)]),
                UnixTimestamp::from_secs(5),
            )?;
            eyre::ensure!(pruned >= 1, "Expected old root chunk to be pruned");
            eyre::Ok(())
        })??;

        db.view_cache(|rtx| -> eyre::Result<()> {
            // Old root should have no entries
            let mut cur_old = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk_old = cur_old.walk_dup(Some(tx_header_old.data_root), None)?;
            eyre::ensure!(
                walk_old.next().transpose()?.is_none(),
                "Old root entries still present"
            );

            // New root should still have its entry
            let mut cur_new = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let walk_new = cur_new.walk_dup(Some(tx_header_new.data_root), None)?;
            let remaining_new: Vec<_> = walk_new.collect::<Result<Vec<_>, _>>()?;
            eyre::ensure!(
                remaining_new.len() == 1,
                "New root entry missing after prune"
            );
            let (_k, entry) = &remaining_new[0];
            eyre::ensure!(
                entry.meta.updated_at.as_secs() >= 5,
                "New root entry should be newer than cutoff"
            );
            Ok(())
        })??;

        Ok(())
    }

    // Ensure prune runs only when cache > 80% capacity.
    #[tokio::test]
    async fn runs_chunk_prune_only_above_capacity_threshold() -> eyre::Result<()> {
        initialize_tracing();
        // Set a small max cache size so 2 chunks (32B each in testing config) reach threshold
        let mut node_config = NodeConfig::testing();
        // First run: below 80% (set to 96B; 64B cache < 76.8B threshold)
        node_config.cache.max_cache_size_bytes = 96;
        let config_below = Config::new_with_random_peer_id(node_config.clone());
        let ingress_address = config_below.irys_signer().address();

        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // Create one data root with two chunks and mark them old to be eligible
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                data_root: DataRoot::random(),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            let c0 = UnpackedChunk {
                data_root: tx_header.data_root,
                data_size: tx_header.data_size,
                data_path: Base64(vec![1]),
                bytes: Base64(vec![7_u8; 8]),
                tx_offset: TxChunkOffset::from(0_u32),
            };
            let c1 = UnpackedChunk {
                data_root: tx_header.data_root,
                data_size: tx_header.data_size,
                data_path: Base64(vec![2]),
                bytes: Base64(vec![8_u8; 8]),
                tx_offset: TxChunkOffset::from(1_u32),
            };
            database::cache_chunk(wtx, &c0)?;
            database::cache_chunk(wtx, &c1)?;
            database::store_ingress_data_hash(
                wtx.inner(),
                tx_header.data_root,
                ingress_address,
                c0.tx_offset,
                irys_types::generate_ingress_data_hash(&c0.bytes.0, ingress_address),
            )?;
            database::store_ingress_data_hash(
                wtx.inner(),
                tx_header.data_root,
                ingress_address,
                c1.tx_offset,
                irys_types::generate_ingress_data_hash(&c1.bytes.0, ingress_address),
            )?;

            // Mark both chunks as very old
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(tx_header.data_root), None)?;
            // Put the aged entries back after the walk: MDBX aborts when this table gets a
            // write while the walk deletes through its cursor (see
            // `put_during_delete_current_walk_aborts` in irys-database).
            let mut aged_entries = Vec::new();
            while let Some((_k, entry)) = walk.next().transpose()? {
                walk.delete_current()?;
                let mut updated = entry;
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
                aged_entries.push(updated);
            }
            for updated in aged_entries {
                wtx.put::<CachedChunksIndex>(tx_header.data_root, updated)?;
            }
            eyre::Ok(())
        })??;

        // Minimal service context
        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config_below.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        // Below-capacity prune: should NOT remove chunks
        let task_below = InnerCacheTask {
            db: db.clone(),
            block_tree_guard: block_tree_guard.clone(),
            block_index_guard: block_index_guard.clone(),
            config: config_below,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx.clone(),
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        task_below.prune_cache(0)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            // Expect both indices still present
            let m0 = irys_database::cached_chunk_meta_by_offset(
                rtx,
                tx_header.data_root,
                TxChunkOffset::from(0_u32),
            )?;
            let m1 = irys_database::cached_chunk_meta_by_offset(
                rtx,
                tx_header.data_root,
                TxChunkOffset::from(1_u32),
            )?;
            eyre::ensure!(
                m0.is_some() && m1.is_some(),
                "Chunks pruned below capacity threshold"
            );
            Ok(())
        })??;

        // Above-capacity prune: set max to 64B so 64B cache > 51.2B threshold.
        // Both offsets are durably stored, so the pass may reclaim them.
        let sm_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let mut node_config2 = node_config;
        node_config2.cache.max_cache_size_bytes = 64;
        let config_above = Config::new_with_random_peer_id(node_config2);
        let task_above = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config: config_above,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: storage_modules_guard_with_durable(
                sm_dir.path().to_path_buf(),
                tx_header.data_root,
                tx_header.data_size,
                &[0, 1],
            )?,
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };
        task_above.prune_cache(0)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let mut cur = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = cur.walk(Some(tx_header.data_root))?;
            eyre::ensure!(
                walk.next().transpose()?.is_none(),
                "Chunks not pruned above capacity threshold"
            );
            Ok(())
        })??;

        Ok(())
    }

    #[tokio::test]
    async fn does_not_prune_data_root_with_local_ingress_proof() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // Create a data root cached via mempool path
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;

        // Set expiry_height to 5 so prune_height > expiry would trigger deletion
        db.update_cache(|wtx| {
            let mut cdr = wtx
                .get::<CachedDataRoots>(tx_header.data_root)?
                .ok_or_else(|| eyre::eyre!("missing CachedDataRoots entry"))?;
            cdr.expiry_height = Some(5);
            wtx.put::<CachedDataRoots>(tx_header.data_root, cdr)?;
            eyre::Ok(())
        })??;

        // Add a LOCAL ingress proof
        let signer = config.irys_signer();
        let local_addr = signer.address();

        db.update_cache(|wtx| {
            let mut ingress_proof = IngressProof::default();
            ingress_proof.data_root = tx_header.data_root;
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &ingress_proof,
                local_addr, // Local address
            )?;
            eyre::Ok(())
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        // Prune with prune_height greater than expiry (6 > 5).
        // Normally this would delete the data root, but because of the local proof, it should stay.
        service_task.prune_data_root_cache(6)?;

        // Verify it was NOT pruned
        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(
                has_root,
                "CachedDataRoots should NOT have been pruned due to local proof"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// Truly anomalous `(None, None)` state: empty `txid_set`, no
    /// `expiry_height`, no local ingress proof. Under the lifetime model
    /// this entry has no remaining tenants and no bound, so the next prune
    /// evicts it. A non-empty `txid_set` keeps the root
    /// (`does_not_prune_cdr_with_pending_sibling_tx`).
    #[tokio::test]
    async fn prunes_truly_anomalous_empty_txid_set() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // Construct a CDR with empty txid_set directly — the only state
        // reachable by `(horizon=None, all_txs_confirmed=true)` after the
        // pending-tx guard.
        let data_root = H256::random();
        let cdr = CachedDataRoot {
            data_size: 64,
            data_size_confirmed: false,
            txid_set: vec![],
            block_set: vec![],
            expiry_height: None,
            cached_at: irys_types::UnixTimestamp::now()?,
        };
        db.update_cache(|wtx| -> eyre::Result<()> {
            wtx.put::<CachedDataRoots>(data_root, cdr)?;
            Ok(())
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        service_task.prune_data_root_cache(1)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(
                !has_root,
                "truly-anomalous CachedDataRoots entry (empty txid_set, no bounds) should be pruned"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// `(None, None)` anomalous-state arm with local-proof exemption:
    /// `all_txs_confirmed == true` (only reachable with empty `txid_set`,
    /// since any non-confirmed tx flips the flag false) combined with
    /// `inclusion_max_height == None` and `expiry_height == None`.  Under
    /// the eviction-candidate rule this fires the `(None, true)` arm and
    /// would evict — except the local-proof check at the bottom of the arm
    /// preserves any entry that still carries a useful commitment.  This
    /// test exercises that exact path: clearing `txid_set` to reach
    /// `all_txs_confirmed = true` without metadata, then asserting survival.
    #[tokio::test]
    async fn does_not_prune_anomalous_none_none_state_with_local_proof() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        // Seed the CDR via the normal path, then clear `txid_set` and
        // `expiry_height` to reach the `(None, None)` + `all_txs_confirmed`
        // state.  An empty `txid_set` is the only way to get
        // `all_txs_confirmed = true` together with `inclusion_max_height = None`:
        // every other path through the prune loop flips the flag to false
        // whenever metadata is missing.
        db.update_cache(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            let mut cdr = wtx
                .get::<CachedDataRoots>(tx_header.data_root)?
                .ok_or_else(|| eyre::eyre!("missing CachedDataRoots entry after seed"))?;
            cdr.txid_set.clear();
            cdr.expiry_height = None;
            wtx.put::<CachedDataRoots>(tx_header.data_root, cdr)?;
            eyre::Ok(())
        })??;

        // Add a local ingress proof for this data_root.
        let signer = config.irys_signer();
        let local_addr = signer.address();
        db.update_cache(|wtx| {
            let mut ingress_proof = IngressProof::default();
            ingress_proof.data_root = tx_header.data_root;
            irys_database::store_external_ingress_proof_checked(wtx, &ingress_proof, local_addr)?;
            eyre::Ok(())
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        // Without the local-proof exemption the (None, None) state would be
        // evicted; with it the entry must survive.
        service_task.prune_data_root_cache(1)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(
                has_root,
                "CachedDataRoots in (None, None) state with local proof should NOT have been pruned"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// Cross-ledger same-`data_root` regression: when two txs share a
    /// `data_root` and one is confirmed (has `included_height`) while the
    /// other is still pending (no metadata row), the CDR must NOT be evicted
    /// on the confirmed tx's height alone — the pending tx still needs the
    /// chunks.  Without the `all_txs_confirmed` guard, `inclusion_max_height`
    /// derived from the confirmed sibling would shadow the now-cleared
    /// `expiry_height` and the CDR would be pruned prematurely, breaking
    /// Publish-promotion liveness when two ledgers reference the same data.
    #[tokio::test]
    async fn does_not_prune_cdr_with_pending_sibling_tx() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        let data_root = H256::random();
        let confirmed_tx_id = H256::random();
        let pending_tx_id = H256::random();
        let make_tx = |id: H256| {
            DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id,
                    data_root,
                    data_size: 64,
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            })
        };
        let confirmed_tx = make_tx(confirmed_tx_id);
        let pending_tx = make_tx(pending_tx_id);

        db.update(|wtx| -> eyre::Result<()> {
            db.update_cache_eyre(|cache_tx| {
                // Both txs land in the same CDR.txid_set via the shared data_root.
                database::cache_data_root(cache_tx, &confirmed_tx, None)?;
                database::cache_data_root(cache_tx, &pending_tx, None)?;
                // The confirmed sibling gets an inclusion height (term-ledger
                // confirmation); the pending tx remains without a metadata row.
                irys_database::set_data_tx_included_height(wtx, &confirmed_tx_id, 5)
                    .map_err(|e| eyre::eyre!("set_data_tx_included_height: {:?}", e))?;
                Ok(())
            })
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        // `prune_height = 100` is well past the confirmed tx's `included_height`
        // = 5.  Without the pending-tx guard the CDR would be evicted on
        // inclusion alone.
        service_task.prune_data_root_cache(100)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(
                has_root,
                "CDR with pending sibling tx (no included_height) must NOT be pruned"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// Regression: `prune_ingress_proofs` must delete only the expired signer's
    /// row, not all proofs for the same `data_root`.
    ///
    /// Sets up three distinct-signer proofs for one `data_root`:
    /// - `signer_a` / `signer_b`: valid anchors (genesis block hash) → not expired
    /// - `signer_c` (not local): invalid anchor (random unknown hash) → expired
    ///
    /// After one pass of `prune_ingress_proofs` the expired proof must be gone
    /// and the two valid ones must still be present.
    #[tokio::test]
    async fn prune_ingress_proofs_preserves_valid_distinct_signer_proofs() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // A shared data_root for all three proofs.
        let data_root = H256::random();
        // A txid that will live in the CDR's txid_set and remain unpromoted.
        let txid = H256::random();

        // Three distinct signer addresses.
        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();
        let addr_c = IrysAddress::random(); // this one will have an expired proof

        // Build the genesis block; its hash is a valid anchor.
        let genesis_block = new_mock_signed_header();
        let valid_anchor = genesis_block.block_hash;
        // A random hash not in the block tree → InvalidAnchor → expired.
        let invalid_anchor = H256::random();

        // Persist CDR + unpromoted tx header + three ingress-proof rows.
        db.update(|wtx| -> eyre::Result<()> {
            db.update_cache_eyre(|cache_tx| {
                // CachedDataRoot with one pending txid (any_unpromoted = true).
                let cdr = CachedDataRoot {
                    data_size: 64,
                    data_size_confirmed: false,
                    txid_set: vec![txid],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::now()?,
                };
                cache_tx.put::<CachedDataRoots>(data_root, cdr)?;

                // Tx header for the pending txid; promoted_height = None (default).
                let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        id: txid,
                        data_root,
                        data_size: 64,
                        ..Default::default()
                    },
                    metadata: DataTransactionMetadata::new(),
                });
                database::insert_tx_header(wtx, &tx_header)?;

                // Two valid proofs (known anchor → not expired).
                let make_proof = |anchor: H256| {
                    let mut p = IngressProof::default();
                    p.data_root = data_root;
                    p.anchor = anchor;
                    p
                };
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(valid_anchor),
                    addr_a,
                )?;
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(valid_anchor),
                    addr_b,
                )?;
                // One expired proof (unknown anchor).
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(invalid_anchor),
                    addr_c,
                )?;

                Ok(())
            })
        })??;

        // Sanity: three proofs inserted.
        let initial_count = db.view_cache_eyre(|rtx| {
            Ok(irys_database::ingress_proofs_by_data_root(rtx, data_root)?.len())
        })?;
        assert_eq!(initial_count, 3, "expected 3 proofs before pruning");

        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        service_task.prune_ingress_proofs()?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let remaining = irys_database::ingress_proofs_by_data_root(rtx, data_root)?;
            assert_eq!(
                remaining.len(),
                2,
                "two valid proofs must survive; got {}",
                remaining.len()
            );
            let addrs: Vec<IrysAddress> = remaining.iter().map(|(_, c)| c.address).collect();
            assert!(addrs.contains(&addr_a), "proof for addr_a must survive");
            assert!(addrs.contains(&addr_b), "proof for addr_b must survive");
            assert!(
                !addrs.contains(&addr_c),
                "expired proof for addr_c must be deleted"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// A full budget spent on the lowest keys must not be the only keys a
    /// later pass can see. Seeking the high key with a budget of one deletes
    /// that proof and leaves the lower one.
    #[tokio::test]
    async fn prune_ingress_proofs_rotates_past_the_lowest_key() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        let low = H256::from([0x01; 32]);
        let high = H256::from([0xFE; 32]);
        let remote = IrysAddress::random();
        let invalid_anchor = H256::random();

        db.update(|wtx| -> eyre::Result<()> {
            db.update_cache_eyre(|cache_tx| {
                for data_root in [low, high] {
                    let txid = H256::random();
                    cache_tx.put::<CachedDataRoots>(
                        data_root,
                        CachedDataRoot {
                            data_size: 64,
                            data_size_confirmed: false,
                            txid_set: vec![txid],
                            block_set: vec![],
                            expiry_height: None,
                            cached_at: irys_types::UnixTimestamp::now()?,
                        },
                    )?;
                    database::insert_tx_header(
                        wtx,
                        &DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                            tx: DataTransactionHeaderV1 {
                                id: txid,
                                data_root,
                                data_size: 64,
                                ..Default::default()
                            },
                            metadata: DataTransactionMetadata::new(),
                        }),
                    )?;
                    let mut proof = IngressProof::default();
                    proof.data_root = data_root;
                    proof.anchor = invalid_anchor;
                    irys_database::store_external_ingress_proof_checked(cache_tx, &proof, remote)?;
                }
                Ok(())
            })
        })??;

        let genesis_block = new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        service_task.prune_ingress_proofs_limited(high, 1)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let low_left = irys_database::ingress_proofs_by_data_root(rtx, low)?.len();
            let high_left = irys_database::ingress_proofs_by_data_root(rtx, high)?.len();
            assert_eq!(low_left, 1, "the lower proof is outside this pass");
            assert_eq!(high_left, 0, "the seek key's proof is the one checked");
            Ok(())
        })??;
        Ok(())
    }

    /// Regression: when the cache is at capacity and this root has no cached
    /// chunk body, a locally-produced expired proof with unpromoted txs must
    /// be deleted (not regenerated). The content-checked delete path
    /// (`delete_ingress_proof_if_unchanged`) must be used so a
    /// concurrently-refreshed proof is preserved. A cached unpersisted body
    /// is a different case: that proof is reanchored.
    ///
    /// Setup:
    /// - `max_cache_size_bytes = 0` → `at_capacity = true` (0 >= 0)
    /// - Local proof (config's signer address) for a data_root that has one
    ///   unpromoted tx; anchor = random hash → expired
    /// - Sibling proof (distinct address) for the same data_root; anchor =
    ///   genesis block hash → NOT expired → must survive
    ///
    /// After one pass of `prune_ingress_proofs`:
    /// - local (expired + at-capacity) proof is deleted
    /// - sibling (valid anchor) proof is preserved
    #[tokio::test]
    async fn prune_ingress_proofs_at_capacity_deletes_local_proof() -> eyre::Result<()> {
        let mut node_config = NodeConfig::testing();
        // Force at_capacity = true: chunk_cache_size (0) >= 0.
        node_config.cache.max_cache_size_bytes = 0;
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        let data_root = H256::random();
        let txid = H256::random();

        // Address of the local signer (derived from NodeConfig::testing()'s fixed key).
        let local_addr = config.irys_signer().address();
        // A different address for the sibling proof.
        let sibling_addr = IrysAddress::random();

        // Build the genesis block; its hash is a valid anchor (not expired).
        let genesis_block = new_mock_signed_header();
        let valid_anchor = genesis_block.block_hash;
        // Unknown anchor → expired.
        let expired_anchor = H256::random();

        db.update(|wtx| -> eyre::Result<()> {
            db.update_cache_eyre(|cache_tx| {
                // CachedDataRoot with one pending (unpromoted) txid.
                let cdr = irys_database::db_cache::CachedDataRoot {
                    data_size: 64,
                    data_size_confirmed: false,
                    txid_set: vec![txid],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::now()?,
                };
                cache_tx.put::<CachedDataRoots>(data_root, cdr)?;

                // Unpromoted tx header (promoted_height = None by default).
                let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        id: txid,
                        data_root,
                        data_size: 64,
                        ..Default::default()
                    },
                    metadata: DataTransactionMetadata::new(),
                });
                database::insert_tx_header(wtx, &tx_header)?;

                // Local proof with expired anchor → at-capacity branch deletes it.
                let mut local_proof = IngressProof::default();
                local_proof.data_root = data_root;
                local_proof.anchor = expired_anchor;
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &local_proof,
                    local_addr,
                )?;

                // Sibling proof with valid anchor → not expired → survives.
                let mut sibling_proof = IngressProof::default();
                sibling_proof.data_root = data_root;
                sibling_proof.anchor = valid_anchor;
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &sibling_proof,
                    sibling_addr,
                )?;

                Ok(())
            })
        })??;

        let initial_count = db.view_cache_eyre(|rtx| {
            Ok(irys_database::ingress_proofs_by_data_root(rtx, data_root)?.len())
        })?;
        assert_eq!(initial_count, 2, "expected 2 proofs before pruning");

        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        service_task.prune_ingress_proofs()?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let remaining = irys_database::ingress_proofs_by_data_root(rtx, data_root)?;
            assert_eq!(
                remaining.len(),
                1,
                "only the sibling proof should survive; got {}",
                remaining.len()
            );
            let addr = remaining[0].1.address;
            assert_eq!(
                addr, sibling_addr,
                "surviving proof must be the sibling (valid anchor)"
            );
            assert_ne!(
                addr, local_addr,
                "local expired proof must have been deleted at capacity"
            );
            Ok(())
        })??;

        Ok(())
    }

    /// `prune_ingress_proofs` must handle ≥2 expired signers for the same data_root in
    /// one pass: both their rows must be deleted while the surviving signer's row is kept.
    ///
    /// Setup — three distinct remote signers for one `data_root`:
    /// - `addr_a`: valid anchor (genesis block hash) → NOT expired → must survive
    /// - `addr_b`: invalid anchor (random unknown hash) → expired → must be deleted
    /// - `addr_c`: invalid anchor (different random hash) → expired → must be deleted
    ///
    /// After one pass of `prune_ingress_proofs`:
    /// - addr_b and addr_c proofs are gone
    /// - addr_a proof is still present
    #[tokio::test]
    async fn prune_ingress_proofs_deletes_two_expired_signers_preserves_one() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(_temp_dir.path(), &_temp_dir.path().join("cache"))?;

        // Shared data_root for all three proofs.
        let data_root = H256::random();
        // A txid present in the CDR's txid_set; promoted_height = None → any_unpromoted = true.
        let txid = H256::random();

        // Three distinct remote signer addresses (none is the local node's address).
        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();
        let addr_c = IrysAddress::random();

        // Genesis block provides the valid anchor.
        let genesis_block = new_mock_signed_header();
        let valid_anchor = genesis_block.block_hash;
        // Two distinct unknown hashes → both proofs expired.
        let expired_anchor_b = H256::random();
        let expired_anchor_c = H256::random();

        db.update(|wtx| -> eyre::Result<()> {
            db.update_cache_eyre(|cache_tx| {
                // CDR with one pending (unpromoted) txid.
                let cdr = CachedDataRoot {
                    data_size: 64,
                    data_size_confirmed: false,
                    txid_set: vec![txid],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::now()?,
                };
                cache_tx.put::<CachedDataRoots>(data_root, cdr)?;

                // Unpromoted tx header (promoted_height = None by default).
                let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        id: txid,
                        data_root,
                        data_size: 64,
                        ..Default::default()
                    },
                    metadata: DataTransactionMetadata::new(),
                });
                database::insert_tx_header(wtx, &tx_header)?;

                let make_proof = |anchor: H256| {
                    let mut p = IngressProof::default();
                    p.data_root = data_root;
                    p.anchor = anchor;
                    p
                };

                // addr_a: valid anchor → not expired → survives.
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(valid_anchor),
                    addr_a,
                )?;
                // addr_b and addr_c: expired anchors → will be pruned.
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(expired_anchor_b),
                    addr_b,
                )?;
                irys_database::store_external_ingress_proof_checked(
                    cache_tx,
                    &make_proof(expired_anchor_c),
                    addr_c,
                )?;

                Ok(())
            })
        })??;

        // Sanity: three proofs inserted.
        let initial_count = db.view_cache_eyre(|rtx| {
            Ok(irys_database::ingress_proofs_by_data_root(rtx, data_root)?.len())
        })?;
        assert_eq!(initial_count, 3, "expected 3 proofs before pruning");

        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        };

        service_task.prune_ingress_proofs()?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            // Exactly one proof must remain — addr_a's.
            let remaining = irys_database::ingress_proofs_by_data_root(rtx, data_root)?;
            assert_eq!(
                remaining.len(),
                1,
                "only addr_a's proof should survive; got {}",
                remaining.len()
            );
            assert_eq!(
                remaining[0].1.address, addr_a,
                "surviving proof must belong to addr_a"
            );

            // Per-key checks using ingress_proof_by_data_root_address.
            assert!(
                irys_database::ingress_proof_by_data_root_address(rtx, data_root, addr_a)?
                    .is_some(),
                "addr_a proof must survive"
            );
            assert!(
                irys_database::ingress_proof_by_data_root_address(rtx, data_root, addr_b)?
                    .is_none(),
                "addr_b expired proof must be deleted"
            );
            assert!(
                irys_database::ingress_proof_by_data_root_address(rtx, data_root, addr_c)?
                    .is_none(),
                "addr_c expired proof must be deleted"
            );
            Ok(())
        })??;

        Ok(())
    }

    // Builds an `InnerCacheTask` whose canonical block-tree window carries
    // `submit_tx_ids` in the Submit ledger of a canonical block, so those txids
    // appear in the tree snapshot the scrub reads. The scrub reads only
    // headers, so the child is sealed with `new_unchecked` and an empty body
    // (as in the `tx_inclusion` test tree builder) rather than fabricating
    // signed matching txs. With no ids, the window is just an empty genesis.
    // Returns the task so tests can drive the scrub directly.
    fn task_with_canonical_submit_txids(
        db: &DatabaseProvider,
        config: Config,
        submit_tx_ids: Vec<H256>,
    ) -> InnerCacheTask {
        use irys_domain::{ChainState, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot};
        use irys_types::{BlockTransactions, SealedBlock};

        let mut genesis_block = new_mock_signed_header();
        genesis_block.height = 0;
        genesis_block.cumulative_diff = 0.into();
        genesis_block.test_sign();
        let mut block_tree = BlockTree::new(&genesis_block, config.consensus.clone());

        if !submit_tx_ids.is_empty() {
            let mut child = new_mock_signed_header();
            child.height = 1;
            child.previous_block_hash = genesis_block.block_hash;
            // Above genesis' 0, so the canonical chain cache follows the child.
            child.cumulative_diff = 1.into();
            // new_mock_header's data_ledgers layout is not guaranteed; target
            // the Submit ledger by id rather than assuming the index.
            let submit = child
                .data_ledgers
                .iter_mut()
                .find(|l| l.ledger_id == DataLedger::Submit as u32)
                .expect("mock header must carry a Submit ledger");
            submit.tx_ids = H256List(submit_tx_ids);
            child.test_sign();

            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(child.clone()),
                BlockTransactions::default(),
            ));
            block_tree
                .add_common(
                    child.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    dummy_ema_snapshot(),
                    ChainState::Onchain,
                )
                .expect("add canonical child");
        }

        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let block_index_guard =
            irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
            storage_modules_guard: test_storage_modules_guard(),
            #[cfg(test)]
            prune_recheck_pause: Arc::new(PruneRecheckPause::new()),
        }
    }

    /// Shared scaffold for the txid-scrub tests: a fresh test DB seeded with
    /// one `CachedDataRoot` carrying `txids` (and no metadata rows). The
    /// `TempDir` guard is part of the return — dropping it deletes the
    /// directory under the open environment.
    fn seed_cdr(
        txids: Vec<H256>,
    ) -> eyre::Result<(
        irys_testing_utils::tempfile::TempDir,
        DatabaseProvider,
        Config,
        H256,
    )> {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(temp_dir.path(), &temp_dir.path().join("cache"))?;

        let data_root = H256::random();
        let cdr = CachedDataRoot {
            data_size: 64,
            data_size_confirmed: false,
            txid_set: txids,
            block_set: vec![],
            expiry_height: Some(100),
            cached_at: irys_types::UnixTimestamp::now()?,
        };
        db.update_cache(|wtx| -> eyre::Result<()> {
            wtx.put::<CachedDataRoots>(data_root, cdr)?;
            Ok(())
        })??;
        Ok((temp_dir, db, config, data_root))
    }

    /// TOCTOU pin for the prune write phases: the shared predicate helpers must veto a delete
    /// when state changed between the RO scan and the gated write — a local proof landing, or
    /// the horizon reopening — because both phases re-evaluate exactly these helpers.
    #[tokio::test]
    async fn prune_recheck_predicate_vetoes_stale_candidates() -> eyre::Result<()> {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let local_addr = config.irys_signer().address();
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider::for_testing(temp_dir.path(), &temp_dir.path().join("cache"))?;

        let data_root = H256::random();
        let cdr = CachedDataRoot {
            data_size: 64,
            data_size_confirmed: false,
            txid_set: vec![],
            block_set: vec![],
            expiry_height: Some(1),
            cached_at: irys_types::UnixTimestamp::now()?,
        };
        db.update_cache(|wtx| -> eyre::Result<()> {
            wtx.put::<CachedDataRoots>(data_root, cdr)?;
            Ok(())
        })??;

        // Scan-time view: past its horizon, no local proof → evictable.
        db.view_cache_eyre(|tx| {
            let cached = tx.get::<CachedDataRoots>(data_root)?.expect("cdr");
            let consensus_tx = db.tx()?;
            let state = InnerCacheTask::cdr_eviction_state(&consensus_tx, &cached, 100)?;
            assert!(state.evictable, "expired unprotected CDR is evictable");
            assert!(!InnerCacheTask::has_local_ingress_proof(
                tx.inner(),
                data_root,
                local_addr
            )?);
            Ok(())
        })?;

        // A local proof lands after the scan; the write-phase re-check must veto.
        db.update_cache(|wtx| -> eyre::Result<()> {
            let mut proof = IngressProof::default();
            proof.data_root = data_root;
            irys_database::store_external_ingress_proof_checked(wtx, &proof, local_addr)?;
            Ok(())
        })??;
        db.view_cache_eyre(|tx| {
            assert!(InnerCacheTask::has_local_ingress_proof(
                tx.inner(),
                data_root,
                local_addr
            )?);
            Ok(())
        })?;

        // The horizon can also reopen (root re-cached with a future expiry): re-evaluated
        // under the write txn, the predicate stops the delete.
        db.update_cache(|wtx| -> eyre::Result<()> {
            let mut cached = wtx.get::<CachedDataRoots>(data_root)?.expect("cdr");
            cached.expiry_height = Some(1_000);
            wtx.put::<CachedDataRoots>(data_root, cached)?;
            Ok(())
        })??;
        db.view_cache_eyre(|tx| {
            let cached = tx.get::<CachedDataRoots>(data_root)?.expect("cdr");
            let consensus_tx = db.tx()?;
            let state = InnerCacheTask::cdr_eviction_state(&consensus_tx, &cached, 100)?;
            assert!(!state.evictable, "revived expiry must veto eviction");
            Ok(())
        })?;
        Ok(())
    }

    // T1 (regression pin): a tx re-confirmed in a canonical-but-unmigrated
    // block (present in the tree's Submit ledger, NO metadata row) must NOT be
    // scrubbed from txid_set. Before the tree-snapshot recheck, the scrub only
    // consulted the migration-time metadata row and would wrongly drop it.
    #[tokio::test]
    async fn keeps_txid_confirmed_in_canonical_tree_without_metadata() -> eyre::Result<()> {
        // CDR whose txid_set carries tx_a; no metadata row is written for it.
        let tx_a = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_a])?;

        // tx_a lives in a canonical tree block's Submit ledger.
        let task = task_with_canonical_submit_txids(&db, config, vec![tx_a]);

        // Queue tx_a for removal; the tree snapshot must keep it.
        let mut by_data_root: HashMap<H256, Vec<H256>> = HashMap::new();
        by_data_root.insert(data_root, vec![tx_a]);
        task.prune_txids_from_cached_data_roots(&by_data_root)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                cached.txid_set.contains(&tx_a),
                "tx re-confirmed in the canonical (unmigrated) tree must be kept in txid_set"
            );
            Ok(())
        })??;

        Ok(())
    }

    // T2 (still removes stale): a tx in neither the canonical tree window nor
    // the metadata table is genuinely stale and must be scrubbed.
    #[tokio::test]
    async fn removes_txid_absent_from_tree_and_metadata() -> eyre::Result<()> {
        let tx_b = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_b])?;

        // The canonical window carries only an unrelated txid, so tx_b is absent.
        let task = task_with_canonical_submit_txids(&db, config, vec![H256::random()]);

        let mut by_data_root: HashMap<H256, Vec<H256>> = HashMap::new();
        by_data_root.insert(data_root, vec![tx_b]);
        task.prune_txids_from_cached_data_roots(&by_data_root)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                !cached.txid_set.contains(&tx_b),
                "stale tx (not in tree, not migrated) must be scrubbed from txid_set"
            );
            Ok(())
        })??;

        Ok(())
    }

    // T3 (migrated still kept): a tx with a metadata row (migration state) and
    // absent from the tree window must be kept — the pre-existing recheck arm.
    #[tokio::test]
    async fn keeps_migrated_txid_with_metadata_row() -> eyre::Result<()> {
        let tx_c = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_c])?;
        // Migration-time metadata row for tx_c.
        db.update(|wtx| -> eyre::Result<()> {
            irys_database::set_data_tx_included_height(wtx, &tx_c, 5)
                .map_err(|e| eyre::eyre!("set_data_tx_included_height: {:?}", e))?;
            Ok(())
        })??;

        // The canonical window does NOT carry tx_c; only the metadata row keeps it.
        let task = task_with_canonical_submit_txids(&db, config, vec![]);

        let mut by_data_root: HashMap<H256, Vec<H256>> = HashMap::new();
        by_data_root.insert(data_root, vec![tx_c]);
        task.prune_txids_from_cached_data_roots(&by_data_root)?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                cached.txid_set.contains(&tx_c),
                "migrated tx (metadata row present) must be kept in txid_set"
            );
            Ok(())
        })??;

        Ok(())
    }

    // T4: tx confirmed while scrub waits on MDBX writer lock must be kept.
    #[test]
    fn spares_txid_confirmed_during_writer_lock_wait() -> eyre::Result<()> {
        use irys_domain::{ChainState, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot};
        use irys_types::{BlockTransactions, SealedBlock};

        // CDR whose txid_set carries tx_x; no metadata row is written for it.
        let tx_x = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_x])?;

        // Empty canonical window: tx_x is NOT confirmed when the scrub is queued.
        let task = task_with_canonical_submit_txids(&db, config, vec![]);

        // Genesis hash, so the confirmation block chains onto it below.
        let genesis_hash = {
            let tree = task.block_tree_guard.read();
            tree.get_canonical_chain()
                .0
                .last()
                .expect("canonical chain carries at least genesis")
                .block_hash()
        };

        // Hold the cache writer gate + MDBX RW tx so the scrub parks inside
        // `update_cache_eyre` (on the gate, then the env lock) before it can read the tree.
        // Locals drop in reverse order: MDBX first, then the gate.
        let gate = irys_database::db::lock_writer_gate(db.cache(), "test.spares_txid_blocker");
        let blocker = db.cache().tx_mut()?;

        // Queue tx_x for removal and run the scrub on another thread; with the
        // fix it parks on the writer path this thread holds, ahead of the tree read.
        let task_thread = task.clone();
        let by: HashMap<H256, Vec<H256>> = HashMap::from([(data_root, vec![tx_x])]);
        let handle =
            std::thread::spawn(move || task_thread.prune_txids_from_cached_data_roots(&by));

        // The confirmation lands during the writer-lock wait: a canonical child
        // carrying tx_x in its Submit ledger (mirrors task_with_canonical_submit_txids).
        {
            let mut tree = task.block_tree_guard.write();
            let mut child = new_mock_signed_header();
            child.height = 1;
            child.previous_block_hash = genesis_hash;
            // Above genesis' 0, so the canonical chain cache follows the child.
            child.cumulative_diff = 1.into();
            // new_mock_header's data_ledgers layout is not guaranteed; target
            // the Submit ledger by id rather than assuming the index.
            let submit = child
                .data_ledgers
                .iter_mut()
                .find(|l| l.ledger_id == DataLedger::Submit as u32)
                .expect("mock header must carry a Submit ledger");
            submit.tx_ids = H256List(vec![tx_x]);
            child.test_sign();

            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(child.clone()),
                BlockTransactions::default(),
            ));
            tree.add_common(
                child.block_hash,
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                dummy_ema_snapshot(),
                ChainState::Onchain,
            )
            .expect("add canonical child");
        }

        // Release MDBX then the gate; the scrub proceeds and reads the tree (now
        // carrying tx_x) inside its own write txn.
        drop(blocker);
        drop(gate);

        handle.join().expect("prune thread panicked")?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                cached.txid_set.contains(&tx_x),
                "tx (re-)confirmed during the writer-lock wait must be kept in txid_set"
            );
            Ok(())
        })??;

        Ok(())
    }

    // T5: try_read defers while the tree write lock is held (no lock-order inversion).
    #[tokio::test]
    async fn defers_scrub_attempt_while_tree_lock_is_write_held() -> eyre::Result<()> {
        // Stale by construction: not in the tree window, no metadata row — an
        // uncontended scrub WOULD remove it.
        let tx_y = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_y])?;

        let task = task_with_canonical_submit_txids(&db, config, vec![]);
        let mut by_data_root: HashMap<H256, Vec<H256>> = HashMap::new();
        by_data_root.insert(data_root, vec![tx_y]);

        // Attempt 1: tree write-held → Deferred, tx_y untouched.
        {
            let _tree_write = task.block_tree_guard.write();
            assert_eq!(
                task.try_prune_txids_once(&by_data_root)?,
                TxidScrubAttempt::Deferred,
                "a write-held tree lock must defer the attempt, not block or scrub"
            );
        }
        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                cached.txid_set.contains(&tx_y),
                "a deferred attempt must not drop entries"
            );
            Ok(())
        })??;

        // Attempt 2: uncontended → Done, the stale entry is scrubbed as normal.
        assert_eq!(
            task.try_prune_txids_once(&by_data_root)?,
            TxidScrubAttempt::Done
        );
        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                !cached.txid_set.contains(&tx_y),
                "the next uncontended attempt must scrub the stale entry"
            );
            Ok(())
        })??;

        Ok(())
    }

    // T6: retrying wrapper completes a deferred one-shot batch once the tree frees.
    #[test]
    fn retries_deferred_scrub_until_tree_lock_frees() -> eyre::Result<()> {
        let tx_z = H256::random();
        let (_temp_dir, db, config, data_root) = seed_cdr(vec![tx_z])?;

        let task = task_with_canonical_submit_txids(&db, config, vec![]);
        let by: HashMap<H256, Vec<H256>> = HashMap::from([(data_root, vec![tx_z])]);

        let tree_write = task.block_tree_guard.write();
        let task_thread = task.clone();
        let handle =
            std::thread::spawn(move || task_thread.prune_txids_from_cached_data_roots(&by));
        // Let the worker reach (and spin on) the deferred attempt, then free
        // the lock; the loop's next attempt must land the scrub.
        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(tree_write);
        handle.join().expect("scrub thread panicked")?;

        db.view_cache(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CDR must still exist");
            eyre::ensure!(
                !cached.txid_set.contains(&tx_z),
                "the retry loop must complete the deferred batch once the tree lock frees"
            );
            Ok(())
        })??;

        Ok(())
    }
}
