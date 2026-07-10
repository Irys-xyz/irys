use crate::chunk_ingress_service::ChunkIngressServiceInner;
use crate::chunk_ingress_service::ingress_proofs::{
    RegenAction, generate_and_store_ingress_proof, reanchor_and_store_ingress_proof,
};
use crate::metrics;
use irys_database::{
    cached_data_root_by_data_root, delete_cached_chunks_by_data_root_older_than,
    delete_ingress_proof_if_unchanged, get_data_tx_metadata, tx_header_by_txid,
};
use irys_database::{
    db::IrysDatabaseExt as _,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{CachedChunks, CachedDataRoots, CompactCachedIngressProof, IngressProofs},
};
use irys_domain::{BlockBoundsError, BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::ingress::CachedIngressProof;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    Config, DataLedger, DataRoot, DatabaseProvider, GIGABYTE, H256, IngressProof,
    LedgerChunkOffset, SendTraced as _, TokioServiceHandle, Traced, UnixTimestamp,
};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{Instrument as _, debug, error, info, info_span, trace, warn};

pub const REGENERATE_PROOFS: bool = true;

/// Maximum evictions per invocation to prevent blocking the service.
/// If this limit is reached, eviction will continue in the next cycle.
const MAX_EVICTIONS_PER_RUN: usize = 10_000;
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
    /// Marks the start of ingress proof generation for the specified data root. Chunks that are
    /// related to this data root should not be pruned if the ingress proof is still being generated.
    NotifyProofGenerationStarted(DataRoot),
    /// Send this when ingress proof generation is completed and the proof has been persisted to the
    /// db. Chunks related to this data root can now be pruned if needed.
    NotifyProofGenerationCompleted(DataRoot),
    /// Requests whether ingress proof generation is currently ongoing for the specified data root.
    RequestIngressProofGenerationState {
        data_root: DataRoot,
        response_sender: Sender<bool>,
    },
    /// Remove specific txids from `CachedDataRoot.txid_set` entries.
    /// Sent by the mempool when txs are pruned (anchor expired) to prevent stale
    /// txid references from blocking publish candidate selection.
    PruneTxidsFromCachedDataRoots(HashMap<H256, Vec<H256>>),
    /// Remove a specific block hash from `CachedDataRoot.block_set`.
    /// `block_set` is a non-trusted hint (consensus now uses
    /// `tx_inclusion::find_canonical_ledger_range`); this action exists so
    /// observers that detect stale entries can scrub them without waiting
    /// for the next confirmation to overwrite the hint.
    ///
    /// **Status: intentionally parked primitive — no live production emitter.**
    /// The original emitter (validator `get_assigned_ingress_proofs` walk
    /// over `block_set` → `AssignedProofBlockMissing` recovery) was deleted
    /// in commit 7da520194 when the validator was rewritten to use
    /// `tx_inclusion::find_canonical_ledger_range` (no longer walks
    /// `block_set`). The action + worker + handler are kept so future
    /// stale-entry observers can wire in without rebuilding the primitive.
    /// The `prune_block_hash_from_block_set_removes_stale_hash` test below
    /// covers this path despite there being no current producer; treat any
    /// "this is unused, delete it" review feedback against this primitive
    /// as already-considered (REVIEW.md P2 #8).
    PruneBlockHashFromBlockSet {
        data_root: DataRoot,
        block_hash: H256,
    },
}

/// Tracks data roots for which ingress proofs are currently being generated
/// to prevent race conditions with chunk pruning
#[derive(Clone, Debug)]
pub struct IngressProofGenerationState {
    inner: Arc<RwLock<HashSet<DataRoot>>>,
}

impl Default for IngressProofGenerationState {
    fn default() -> Self {
        Self::new()
    }
}

impl IngressProofGenerationState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn mark_generating(&self, data_root: DataRoot) -> bool {
        self.inner
            .write()
            .expect("expected to acquire a lock for an ingress proof generation state")
            .insert(data_root)
    }

    pub fn unmark_generating(&self, data_root: DataRoot) {
        self.inner
            .write()
            .expect("expected to acquire a lock for an ingress proof generation state")
            .remove(&data_root);
    }

    pub fn is_generating(&self, data_root: DataRoot) -> bool {
        self.inner
            .read()
            .expect("expected to acquire a lock for an ingress proof generation state")
            .contains(&data_root)
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
                // Genesis block (height 0) never enters block_index as it has no submit ledger
                // data, use saturating_sub (defensive).
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
            // Genesis block (height 0) never enters canonical chain with submit ledger data,
            // but use saturating_sub (defensive) for consistency
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
            self.db.view_eyre(|tx| {
                Ok((
                    get_cache_size::<CachedChunks, _>(tx, self.config.consensus.chunk_size)?,
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
                    "Cache above max capacity! size: {} max: {}",
                    &chunk_cache_size, &max_cache_size_bytes
                )
            }
            // Then, prune chunks that no longer have active ingress proofs
            self.prune_chunks_without_active_ingress_proofs()?;
        } else {
            debug!("Cache under target capacity, skipping chunk pruning");
        }

        Ok(())
    }

    /// Prunes cached chunks for data roots that have no ingress proofs.
    /// Since `prune_ingress_proofs()` runs immediately before this and removes
    /// expired/invalid proofs, any remaining proof entry is treated as active.
    /// Data roots currently undergoing proof generation are skipped to avoid races.
    #[tracing::instrument(level = "trace", skip_all)]
    fn prune_chunks_without_active_ingress_proofs(&self) -> eyre::Result<()> {
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

        let tx = self.db.tx()?;

        // Collect candidate data roots from CachedDataRoots
        let mut cdr_cursor = tx.cursor_read::<CachedDataRoots>()?;
        let mut cdr_walk = cdr_cursor.walk(None)?;

        // Limit total evictions to avoid long pauses
        let mut evictions_performed: usize = 0;

        // We'll do deletions in a write tx per batch to keep lock times short
        let mut pending_roots: Vec<DataRoot> = Vec::new();

        // TODO: try to deprioritise data_roots that almost have all their chunks, as we want as many full data_roots as possible so we can provide proofs
        while let Some((data_root, _cached)) = cdr_walk.next().transpose()? {
            if evictions_performed >= MAX_EVICTIONS_PER_RUN {
                warn!(
                    chunk.evictions_performed = evictions_performed,
                    "Hit max eviction limit in prune_chunks_without_active_ingress_proofs, will continue next cycle"
                );
                break;
            }

            // Skip if an ingress proof is actively being generated for this root
            if self.ingress_proof_generation_state.is_generating(data_root) {
                debug!(ingress_proof.data_root = ?data_root, "Skipping chunk prune due to active proof generation");
                continue;
            }

            // Presence of at least one proof indicates the data root is active
            let mut proofs_cursor = tx.cursor_read::<IngressProofs>()?;
            let mut has_any_local_proof = false;
            let mut walker = proofs_cursor.walk(Some(data_root))?;
            while let Some((key, compact)) = walker.next().transpose()? {
                // Walked all records for this data root
                if key != data_root {
                    break;
                }
                // Found at least one proof, mark as active
                if compact.0.address == local_address {
                    has_any_local_proof = true;
                    break;
                }
            }

            if !has_any_local_proof {
                pending_roots.push(data_root);
            }

            // Commit a small batch to avoid large transactions
            if pending_roots.len() >= 256 {
                // Span attributes any libmdbx writer-lock stall warning fired
                // during begin_rw_txn to libmdbx_rw_tx_lock_stalls_total
                // {scope="irys-consensus"} (see crates/utils/utils/src/mdbx_metrics.rs).
                let _span = info_span!(
                    irys_utils::MDBX_RW_TX_SPAN,
                    db_scope = irys_utils::DB_SCOPE_IRYS_CONSENSUS
                )
                .entered();
                let write_tx = self.db.tx_mut()?;
                for root in pending_roots.drain(..) {
                    trace!(
                        chunk.data_root = ?root,
                        "Pruning chunks for data root without active proofs"
                    );
                    let pruned = delete_cached_chunks_by_data_root_older_than(
                        &write_tx,
                        root,
                        delete_chunks_older_than,
                    )?;
                    if pruned > 0 {
                        evictions_performed = evictions_performed.saturating_add(1);
                        debug!(chunk.data_root = ?root, chunk.pruined_chunks = pruned, "Pruned chunks for data root without active proofs");
                    }
                }
                write_tx.commit()?;
            }
        }

        // Flush any remaining pending deletions
        if !pending_roots.is_empty() {
            let _span = info_span!(
                irys_utils::MDBX_RW_TX_SPAN,
                db_scope = irys_utils::DB_SCOPE_IRYS_CONSENSUS
            )
            .entered();
            let write_tx = self.db.tx_mut()?;
            for root in pending_roots.drain(..) {
                let pruned = delete_cached_chunks_by_data_root_older_than(
                    &write_tx,
                    root,
                    delete_chunks_older_than,
                )?;
                if pruned > 0 {
                    evictions_performed = evictions_performed.saturating_add(1);
                    debug!(chunk.data_root = ?root, chunk.pruned_chunks = pruned, "Pruned chunks for data root without active proofs");
                }
            }
            write_tx.commit()?;
        }

        info!(
            chunk.eviction_batches = evictions_performed,
            "Completed chunk pruning pass"
        );
        Ok(())
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
        let mut chunks_pruned: u64 = 0;
        let mut eviction_count: usize = 0;
        let mut evictions: Vec<EvictionRecord> = Vec::new();
        let _span = info_span!(
            irys_utils::MDBX_RW_TX_SPAN,
            db_scope = irys_utils::DB_SCOPE_IRYS_CONSENSUS
        )
        .entered();
        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<CachedDataRoots>()?;
        // Seed the cursor with a random data_root so MDBX seeks to the
        // nearest key; without this, a stable backlog of CDRs protected by
        // the pending-tx / local-proof exemptions re-scans from the front
        // every cycle and starves later entries.  Mirrors the TODO at
        // `prune_ingress_proofs` (~L696).  When the first walk runs out of
        // entries without hitting `MAX_EVICTIONS_PER_RUN`, fall through to
        // a second walk from the beginning so total coverage stays
        // wrap-around-complete (otherwise tests + small tables with all
        // keys above the random seek would silently scan zero entries).
        let seek_key: DataRoot = H256::random();
        let mut walker = cursor.walk(Some(seek_key))?;
        let mut wrapped = false;
        loop {
            while let Some((data_root, cached)) = walker.next().transpose()? {
                if eviction_count >= MAX_EVICTIONS_PER_RUN {
                    warn!(
                        evictions_performed = eviction_count,
                        "Hit max eviction limit in prune_data_root_cache, will continue next cycle"
                    );
                    break;
                }
                // Pruning horizon priority: canonical tx inclusion > expiry height > evict-anomalous.
                //
                // CDR lifetime is bounded by either (a) `expiry_height` set at tx
                // ingress (`cache_data_root_with_expiry`), or (b) the tx's
                // `included_height`, written at migration by
                // `BlockMigrationService::persist_block` (`write_data_ledger_metadata`).
                // `expiry_height` is cleared earlier, at confirmation, by
                // `persist_metadata` Phase 3 (`update_data_root_block_set`).  The
                // local-proof exemption below extends (b) indefinitely while a
                // local ingress proof is present.
                //
                // The `(None, None)` arm is reachable only when a CDR has lost
                // both bounds: a reorg cleared `included_height` for every tx in
                // `txid_set` after the confirmation already cleared
                // `expiry_height`, and the mempool re-anchor path did not restore
                // expiry (e.g. orphan resubmission failed).  Under the lifetime
                // model such an entry should not exist, so treat it as a
                // candidate for eviction — but keep the local-proof exemption so
                // an entry that still carries a useful commitment survives.
                let mut inclusion_max_height: Option<u64> = None;
                let mut all_txs_confirmed = true;
                for tx_id in cached.txid_set.iter() {
                    match irys_database::get_data_tx_metadata(&write_tx, tx_id)?
                        .and_then(|m| m.included_height)
                    {
                        Some(h) => {
                            inclusion_max_height =
                                Some(inclusion_max_height.map_or(h, |x| x.max(h)));
                        }
                        // No metadata row, or row exists with `included_height = None`:
                        // this tx (sharing `data_root` with the rest of `txid_set`) is
                        // still pending.  `inclusion_max_height` alone would prematurely
                        // prune chunks the pending tx still needs — e.g. a Publish-
                        // promotion sibling of an already-confirmed term-ledger tx that
                        // shares the same `data_root`.
                        //
                        // Once any tx is pending, the horizon below ignores
                        // `inclusion_max_height` and defers to `expiry_height`, so
                        // further metadata reads are wasted DB work inside the open
                        // write tx.  Bail early.
                        None => {
                            all_txs_confirmed = false;
                            break;
                        }
                    }
                }

                // Horizon policy:
                //  - All txs confirmed: use the latest `included_height` as the
                //    boundary (falling back to `expiry_height`, then `(None, None)`
                //    anomalous-eviction).
                //  - Any tx still pending: defer to `expiry_height`.  An already-
                //    confirmed sibling's `included_height` does NOT bind the
                //    pending tx's chunk lifetime; using it as the horizon would
                //    prematurely prune chunks the pending tx still needs.  If
                //    expiry was already cleared by the sibling confirmation,
                //    `horizon` is `None` and the eviction-candidate arm protects
                //    the entry until the pending tx either confirms or is dropped
                //    by mempool TTL.
                let horizon = if all_txs_confirmed {
                    match (inclusion_max_height, cached.expiry_height) {
                        (Some(h), _) => Some(h),
                        (None, Some(e)) => Some(e),
                        (None, None) => None,
                    }
                } else {
                    cached.expiry_height
                };

                // Decide whether this entry is a candidate for eviction.
                //  * horizon present: the historical "past the bound" check.
                //  * horizon absent + all txs confirmed: truly anomalous state
                //    (empty `txid_set`, or all txs confirmed-and-orphaned with
                //    expiry already cleared) — evict unless the local-proof
                //    exemption below applies.
                //  * horizon absent + pending tx remains: protect (see horizon
                //    policy above).
                let is_eviction_candidate = match (horizon, all_txs_confirmed) {
                    (Some(max_height), _) => max_height < prune_height,
                    (None, true) => true,
                    (None, false) => false,
                };

                trace!(
                    data_root.data_root = ?data_root,
                    horizon = ?horizon,
                    prune_height,
                    is_eviction_candidate,
                    "Processing data root for prune evaluation"
                );

                if is_eviction_candidate {
                    // Check for locally generated ingress proof
                    let mut proofs_cursor = write_tx.cursor_read::<IngressProofs>()?;
                    let mut has_local_proof = false;
                    let mut proof_walker = proofs_cursor.walk(Some(data_root))?;
                    while let Some((key, compact)) = proof_walker.next().transpose()? {
                        if key != data_root {
                            break;
                        }
                        if compact.0.address == local_addr {
                            has_local_proof = true;
                            break;
                        }
                    }

                    if has_local_proof {
                        trace!(
                            data_root.data_root = ?data_root,
                            horizon = ?horizon,
                            "Skipping prune for data root with locally generated ingress proof"
                        );
                        continue;
                    }

                    // Capture txid_set_size before `cached` is partially moved below.
                    let txid_set_size = cached.txid_set.len();
                    let expiry_height = cached.expiry_height;

                    write_tx.delete::<IngressProofs>(data_root, None)?;
                    chunks_pruned = chunks_pruned
                        .saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
                    write_tx.delete::<CachedDataRoots>(data_root, None)?;
                    eviction_count += 1;

                    evictions.push(EvictionRecord {
                        data_root,
                        block_set: cached.block_set,
                        txid_set_size,
                        inclusion_max_height,
                        expiry_height,
                        horizon,
                    });
                }
            }
            if wrapped || eviction_count >= MAX_EVICTIONS_PER_RUN {
                break;
            }
            // First walk ran from `Some(seek_key)` to end without hitting the
            // eviction cap.  Wrap around and scan the prefix `[start,
            // seek_key)` so coverage is complete.  Without this fall-through
            // a small table whose keys all sort above `seek_key` would
            // silently scan zero entries this cycle.
            wrapped = true;
            walker = cursor.walk(None)?;
        }
        write_tx.commit()?;

        // Post-commit: only now is it safe to report what actually persisted.
        // `has_local_proof` is hardcoded false because the eviction branch
        // `continue`s on `has_local_proof == true` before reaching this buffer.
        for entry in &evictions {
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

    /// Spawns a background thread to remove specific txids from `CachedDataRoot.txid_set` entries.
    ///
    /// Called when the mempool prunes expired txs or the reorg handler observes
    /// a failed orphan re-ingress.  In both cases the *decision to scrub* is
    /// made on stale state (either pre-TTL or pre-reorg-cleanup); by the time
    /// this task runs the world may have moved on — most importantly, the tx
    /// may have been re-confirmed by a new block that arrived in the gap.
    ///
    /// `txid_set` is **authoritative state** (see [`CachedDataRoot::txid_set`]
    /// docs).  Removing an entry whose tx has since been (re-)confirmed
    /// would make the prune loop miss that confirmation's chunk-retention
    /// constraint and evict the CDR before chunk migration completes,
    /// destroying chunks that are still needed.  Per-txid recheck in
    /// [`Self::prune_txids_from_cached_data_roots`] closes that race across
    /// both confirmation states, keeping a txid if either holds:
    ///   - `IrysDataTxMetadata[tx_id]` is `Some`: the tx is migrated
    ///     (canonical metadata is written only at migration).
    ///   - the tx_id appears in the canonical block-tree window: the tx is
    ///     confirmed but not yet migrated, so no metadata row exists yet.
    ///     Only the canonical chain counts — "(re-)confirmed" means on the
    ///     branch this node currently follows.
    /// Together they restore the pre-migration retention contract.
    ///
    /// Residual race the recheck does not close: a peer gossips the tx back
    /// in during the scrub window and `cache_data_root` re-adds it to
    /// `txid_set`, but the tx has NOT confirmed anywhere — no metadata row
    /// and not in the canonical tree window.  The scrub would drop it.
    /// Bounded by mempool TTL re-population and `cache_data_root` idempotency
    /// on the next ingress.  Acceptable for the current architecture; revisit
    /// if the mempool grows a strong "currently-pending" signal that can be
    /// consulted from MDBX context.
    ///
    /// Uses a background thread (matching `spawn_pruning_task`) to avoid
    /// blocking the actor loop.  Sends `PruneTxidsCompleted` when done so
    /// the service can drive the queue.
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

    /// Scrub body for [`Self::spawn_prune_txids_task`]: removes the queued
    /// txids from their `CachedDataRoot.txid_set` entries, keeping any that has
    /// been (re-)confirmed since the scrub was queued. See that method's docs
    /// for the full race rationale.
    ///
    /// Retries until an attempt lands: the batch is one-shot (the mempool sends
    /// it on tx-removal and reorg events, with no later re-producer), so
    /// dropping it when [`Self::try_prune_txids_once`] defers on block-tree
    /// contention would leave stale `txid_set` entries pinning cached data
    /// roots indefinitely. Each attempt commits its own (possibly empty) write
    /// txn, so the MDBX writer lock is released between retries. Runs on the
    /// scrub worker thread, where sleeping is harmless.
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

    /// One txid-scrub attempt inside a single write txn. Returns
    /// [`TxidScrubAttempt::Deferred`] — having written nothing — when the
    /// block-tree lock is write-held; the caller retries.
    fn try_prune_txids_once(
        &self,
        by_data_root: &HashMap<H256, Vec<H256>>,
    ) -> eyre::Result<TxidScrubAttempt> {
        self.db.update_eyre(|db_tx| {
            // Snapshot the txids carried by the canonical block-tree window AFTER
            // the write txn has taken the MDBX writer lock, so the
            // confirmed-but-unmigrated arm below is read at the same instant as
            // the migrated arm (`get_data_tx_metadata` on `db_tx`). Capturing it
            // before `update_eyre` would miss a tx (re-)confirmed while this task
            // waited on the serialized writer lock: such a tx has no
            // `IrysDataTxMetadata` row yet (written only at migration) and would
            // be absent from a pre-wait snapshot, so both recheck arms would drop
            // a live tx. Reading only the canonical chain scopes "(re-)confirmed"
            // to the branch this node currently follows.
            //
            // The tree read MUST be non-blocking here. This closure holds the
            // serialized MDBX writer lock, and `on_block_validation_finished`
            // takes the two locks in the OPPOSITE order on its deep-reorg
            // branch: it holds the tree WRITE lock while
            // `recover_from_network_partition` truncates the block index
            // inside a consensus-DB write txn (tree-write, then writer-lock).
            // A blocking `read()` here (writer-lock, then tree-read) would
            // complete that AB-BA cycle and deadlock the node. On contention
            // this attempt reports `Deferred` and the retry loop above re-runs
            // it once the empty txn has released the writer lock — keeping a
            // doomed entry a little longer is harmless, deleting a live one is
            // not.
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
                    for tx_ids in entry.header().get_data_ledger_tx_ids().into_values() {
                        set.extend(tx_ids);
                    }
                }
                set
            };

            for (data_root, txids_to_remove) in by_data_root {
                let Some(mut cached) = cached_data_root_by_data_root(db_tx, *data_root)? else {
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
                    if get_data_tx_metadata(db_tx, tx_id)?.is_some() || tree_set.contains(tx_id) {
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

    /// Removes a single stale block hash from `CachedDataRoot.block_set`.
    ///
    /// Called when validation finds `get_ledger_range` returning `Ok(None)` for a
    /// hash stored in the set.  Runs on a background thread to avoid blocking the
    /// actor loop (mirrors `spawn_prune_txids_task`).  No completion signal is
    /// needed because block-hash pruning does not need to be serialised.
    fn spawn_prune_block_hash_task(&self, data_root: DataRoot, block_hash: H256) {
        let clone = self.clone();
        // Deliberately NO catch_unwind here: this task has no completion signal
        // to forward to the actor loop. A panic loses one block_set entry but
        // doesn't jam the queue (contrast with spawn_prune_txids_task,
        // spawn_pruning_task, and spawn_epoch_processing, which all wrap their
        // body in catch_unwind to protect their completion signals).
        std::thread::spawn(move || {
            let result = clone.db.update_eyre(|db_tx| {
                let Some(mut cached) = cached_data_root_by_data_root(db_tx, data_root)? else {
                    return Ok(());
                };
                let before_len = cached.block_set.len();
                cached.block_set.retain(|h| h != &block_hash);
                if cached.block_set.len() < before_len {
                    debug!(
                        data_root = %data_root,
                        block_hash = %block_hash,
                        "Pruned stale block hash from CachedDataRoot.block_set"
                    );
                    db_tx.put::<CachedDataRoots>(data_root, cached)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                warn!(
                    data_root = %data_root,
                    block_hash = %block_hash,
                    "Failed to prune stale block hash from CachedDataRoot.block_set: {}",
                    e
                );
            }
        });
    }

    /// Scans ingress proofs with capacity-aware deletion and regeneration:
    /// - (a) Promoted + expired: delete
    /// - (b) Unpromoted + expired + at capacity: delete
    /// - (c) Unpromoted + expired + under capacity + local author: regenerate
    #[tracing::instrument(level = "trace", skip_all)]
    fn prune_ingress_proofs(&self) -> eyre::Result<()> {
        let signer = self.config.irys_signer();
        let local_addr = signer.address();
        let tx = self.db.tx()?;
        let mut cursor = tx.cursor_read::<IngressProofs>()?;
        // TODO: we can randomise the start of the cursor by providing a random key. MDBX will seek to the neareset key if it doesn't exist.
        // we might want to do this to prevent scanning over just the first `MAX_PROOF_CHECKS_PER_RUN` valid entries.
        let mut walker = cursor.walk(None)?;
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
            get_cache_size::<CachedChunks, _>(&tx, self.config.consensus.chunk_size)?;
        let at_capacity = chunk_cache_size >= *max_cache_size_bytes;
        debug!(
            "Cache is {} at capacity - capacity: {}B, size {}B ({} chunks)",
            if at_capacity { "" } else { "not" },
            &max_cache_size_bytes,
            chunk_cache_size,
            &chunk_count
        );

        while let Some((data_root, compact)) = walker.next().transpose()? {
            if processed >= MAX_PROOF_CHECKS_PER_RUN {
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
                if let Some(tx_header) = tx_header_by_txid(&tx, txid)?
                    && tx_header.promoted_height().is_none()
                {
                    any_unpromoted = true;
                    debug!(ingress_proof.data_root = ?data_root, tx.id = ?tx_header.id, "Found unpromoted tx for data root");
                    break;
                }
            }

            let is_locally_produced = address == local_addr;

            if at_capacity {
                // Unpromoted + expired + at capacity: delete
                to_delete.push((data_root, compact, false));
                debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = true, "Marking expired proof for deletion (at capacity)");
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

        // Release the scan's read transaction (and its walker/cursor) before the
        // per-proof write-tx loop below. Holding a long-lived reader open across
        // up to MAX_PROOF_CHECKS_PER_RUN short write txns would pin the MVCC
        // snapshot and hold back MDBX free-page reclamation; nothing past the scan
        // reads from `tx` (the delete/reanchor/regen loops open their own txns).
        drop(walker);
        drop(cursor);
        drop(tx);

        // Delete expired proofs — per-signer to avoid wiping other signers' rows on the same
        // data_root. Content-aware delete closes the TOCTOU window: a fresh proof stored
        // between the scan and this delete will not match the scanned value and is preserved.
        if !to_delete.is_empty() {
            let mut deleted_count = 0_usize;
            for (root, scanned, is_orphan) in to_delete.iter() {
                match self.db.update_eyre(|rw_tx| {
                    // Orphan deletions were scheduled because the data root had no
                    // `CachedDataRoots` entry at scan time. Re-check inside the write tx:
                    // if the root was re-cached since the scan, keep its proof.
                    if *is_orphan && cached_data_root_by_data_root(rw_tx, *root)?.is_some() {
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
            if REGENERATE_PROOFS {
                if let Err(error) = reanchor_and_store_ingress_proof(
                    &self.block_tree_guard,
                    &self.db,
                    &self.config,
                    &signer,
                    proof,
                    &self.gossip_broadcast,
                    &self.cache_sender,
                ) {
                    if error.is_benign() {
                        debug!(ingress_proof.data_root = ?proof, "Skipped ingress proof reanchoring: {error}");
                    } else {
                        warn!(ingress_proof.data_root = ?proof, "Failed to regenerate ingress proof: {error}");
                    }
                }
            } else {
                debug!(
                    ingress_proof.data_root = ?proof.data_root,
                    "Skipping reanchoring of ingress proof due to REGENERATE_PROOFS = false"
                );
                // Content-checked delete, same TOCTOU guard as the `to_delete`
                // loop: only remove the local proof if it is still the one we
                // scanned, so a proof refreshed since the scan is preserved.
                let scanned = CompactCachedIngressProof(CachedIngressProof {
                    address: local_addr,
                    proof: proof.clone(),
                });
                if let Err(e) = self.db.update_eyre(|rw_tx| {
                    delete_ingress_proof_if_unchanged(rw_tx, proof.data_root, scanned)
                }) {
                    warn!(ingress_proof.data_root = ?proof.data_root, "Failed to remove ingress proof: {e}");
                }
            }
        }

        for proof in to_regen.iter() {
            if REGENERATE_PROOFS {
                if let Err(error) = generate_and_store_ingress_proof(
                    &self.block_tree_guard,
                    &self.db,
                    &self.config,
                    proof.data_root,
                    None,
                    &self.gossip_broadcast,
                    &self.cache_sender,
                ) {
                    if error.is_benign() {
                        debug!(ingress_proof.data_root = ?proof.data_root, "Skipped ingress proof regeneration: {error}");
                    } else {
                        warn!(ingress_proof.data_root = ?proof.data_root, "Failed to regenerate ingress proof: {error}");
                    }
                }
            } else {
                debug!(
                    ingress_proof.data_root = ?proof.data_root,
                    "Regeneration disabled, removing ingress proof for data root"
                );
                // Content-checked delete, same TOCTOU guard as the `to_delete`
                // loop: only remove the local proof if it is still the one we
                // scanned, so a proof refreshed since the scan is preserved.
                let scanned = CompactCachedIngressProof(CachedIngressProof {
                    address: local_addr,
                    proof: proof.clone(),
                });
                if let Err(e) = self.db.update_eyre(|rw_tx| {
                    delete_ingress_proof_if_unchanged(rw_tx, proof.data_root, scanned)
                }) {
                    warn!(ingress_proof.data_root = ?proof.data_root, "Failed to remove ingress proof: {e}");
                }
            }
        }

        if !to_reanchor.is_empty() {
            info!(
                proofs.regenerated = to_reanchor.len(),
                "Reanchored expired local ingress proofs (under capacity)"
            );
        }

        Ok(())
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
                    ingress_proof_generation_state: IngressProofGenerationState::new(),
                    cache_sender,
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
            CacheServiceAction::NotifyProofGenerationStarted(data_root) => {
                self.cache_task
                    .ingress_proof_generation_state
                    .mark_generating(data_root);
            }
            CacheServiceAction::NotifyProofGenerationCompleted(data_root) => {
                self.cache_task
                    .ingress_proof_generation_state
                    .unmark_generating(data_root);
            }
            CacheServiceAction::RequestIngressProofGenerationState {
                data_root,
                response_sender,
            } => {
                let is_generating = self
                    .cache_task
                    .ingress_proof_generation_state
                    .is_generating(data_root);
                if let Err(e) = response_sender.send(is_generating) {
                    warn!(custom.error = ?e, "Failed to respond to RequestIngressProofGenerationState");
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
            CacheServiceAction::PruneBlockHashFromBlockSet {
                data_root,
                block_hash,
            } => {
                self.cache_task
                    .spawn_prune_block_hash_task(data_root, block_hash);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, database,
        db_cache::CachedDataRoot,
        open_or_create_db,
        tables::{CachedChunks, CachedChunksIndex, CachedDataRoots, IrysTables},
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
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::{Arc, RwLock};

    // This test prevents a regression of bug: mempool-only data roots (with empty block_set field)
    // are pruned once prune_height > 0 and they should not be pruned!
    //
    // Real prod ingress goes through `cache_data_root_with_expiry`, which sets
    // `expiry_height` to `anchor + tx_anchor_expiry_depth`.  Direct callers of
    // `cache_data_root(_, _, None)` (this fixture) leave
    // `expiry_height = None`.  We set it manually here to mirror what the
    // mempool ingress path produces, so the test exercises the realistic
    // "unconfirmed entry with expiry in the future" state rather than the
    // anomalous (None, None) state.
    #[tokio::test]
    async fn does_not_prune_unconfirmed_data_roots() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;

        // Mirror `cache_data_root_with_expiry`: set expiry_height to a future
        // height so the entry has a valid lifetime bound (pre-confirmation arm).
        db.update(|wtx| {
            let mut cdr = wtx
                .get::<CachedDataRoots>(tx_header.data_root)?
                .ok_or_else(|| eyre::eyre!("missing CachedDataRoots entry"))?;
            cdr.expiry_height = Some(100);
            wtx.put::<CachedDataRoots>(tx_header.data_root, cdr)?;
            eyre::Ok(())
        })??;

        let chunk = UnpackedChunk {
            data_root: tx_header.data_root,
            data_size: tx_header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![0_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            eyre::Ok(())
        })??;

        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
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
            },
            pruning_running: false,
            pruning_queue: VecDeque::new(),
            epoch_running: false,
            epoch_queue: VecDeque::new(),
            txid_prune_running: false,
            txid_prune_queue: VecDeque::new(),
        };

        // Invoke pruning with prune_height > 0 which should NOT delete mempool-only roots
        service.cache_task.prune_data_root_cache(1)?;

        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(has_root, "CachedDataRoots was prematurely pruned");
            Ok(())
        })??;

        Ok(())
    }

    // Ensure that an expired, never-confirmed data root (expiry_height set; empty block_set)
    // is pruned when prune_height exceeds expiry.
    #[tokio::test]
    async fn prunes_expired_never_confirmed_data_root() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;

        // Set expiry_height to 5 (arbitrary) so prune_height > expiry will trigger deletion
        db.update(|wtx| {
            let mut cdr = wtx
                .get::<CachedDataRoots>(tx_header.data_root)?
                .ok_or_else(|| eyre::eyre!("missing CachedDataRoots entry"))?;
            cdr.expiry_height = Some(5);
            wtx.put::<CachedDataRoots>(tx_header.data_root, cdr)?;
            eyre::Ok(())
        })??;

        // Sanity: it exists
        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
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
            },
            pruning_running: false,
            pruning_queue: VecDeque::new(),
            epoch_running: false,
            epoch_queue: VecDeque::new(),
            txid_prune_running: false,
            txid_prune_queue: VecDeque::new(),
        };

        // Prune with prune_height greater than expiry (6 > 5) -> should delete
        service.cache_task.prune_data_root_cache(6)?;

        // Verify it was pruned
        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(!has_root, "CachedDataRoots should have been pruned");
            Ok(())
        })??;

        Ok(())
    }

    // Chunks should remain when there is an active ingress proof present for the data_root.
    #[tokio::test]
    async fn does_not_prune_chunks_with_active_proof() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create tx header + data root + chunk
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;
        let chunk = UnpackedChunk {
            data_root: tx_header.data_root,
            data_size: tx_header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![1_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            eyre::Ok(())
        })??;

        // Insert a (non-expired) ingress proof entry for the data root so pruning treats it as active
        db.update(|wtx| {
            let mut ingress_proof = IngressProof::default();
            ingress_proof.data_root = tx_header.data_root;
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &ingress_proof,
                irys_types::IrysAddress::random(),
            )?;
            eyre::Ok(())
        })??;

        // Setup minimal service context
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
        };

        // Execute chunk-only pruning (proofs present so should skip deletion)
        service_task.prune_chunks_without_active_ingress_proofs()?;

        // Verify chunk still exists
        db.view(|rtx| -> eyre::Result<()> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(tx_header.data_root))?;
            let has_index = walk.next().transpose()?.is_some();
            eyre::ensure!(
                has_index,
                "CachedChunksIndex entry missing after prune but proof was active"
            );
            // Attempt to resolve chunk from index metadata
            if let Some((_, idx_entry)) = rtx
                .cursor_dup_read::<CachedChunksIndex>()?
                .seek_exact(tx_header.data_root)?
            {
                let meta: irys_database::db_cache::CachedChunkIndexMetadata = idx_entry.into();
                let chunk_entry = rtx.get::<CachedChunks>(meta.chunk_path_hash)?;
                eyre::ensure!(
                    chunk_entry.is_some(),
                    "CachedChunks value missing after prune but proof was active"
                );
            }
            Ok(())
        })??;
        Ok(())
    }

    // Chunks should be pruned when there is no ingress proof for the data_root.
    #[tokio::test]
    async fn prunes_chunks_without_any_proof() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;
        let chunk = UnpackedChunk {
            data_root: tx_header.data_root,
            data_size: tx_header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![2_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
            // Mark the cached chunk as very old so it qualifies for age-based pruning
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(tx_header.data_root), None)?;
            while let Some((_k, entry)) = walk.next().transpose()? {
                // Delete the current index entry and reinsert with an old timestamp
                wtx.delete::<CachedChunksIndex>(tx_header.data_root, Some(entry.clone()))?;
                let mut updated = entry.clone();
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
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
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
        };

        // Execute chunk pruning (no proofs -> should delete)
        service_task.prune_chunks_without_active_ingress_proofs()?;

        // Verify chunk removed
        db.view(|rtx| -> eyre::Result<()> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(tx_header.data_root))?;
            let index_entry = walk.next().transpose()?;
            eyre::ensure!(
                index_entry.is_none(),
                "CachedChunksIndex entry still present but should have been pruned"
            );
            Ok(())
        })??;
        Ok(())
    }

    // Chunks older than threshold should be deleted while newer ones should remain.
    #[tokio::test]
    async fn prunes_only_chunks_older_than_threshold() -> eyre::Result<()> {
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
        db.update(|wtx| {
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

        db.update(|wtx| {
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
        db.view(|rtx| -> eyre::Result<()> {
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

        db.update(|wtx| {
            let pruned = delete_cached_chunks_by_data_root_older_than(
                wtx,
                tx_header_old.data_root,
                UnixTimestamp::from_secs(5),
            )?;
            eyre::ensure!(pruned >= 1, "Expected old root chunk to be pruned");
            eyre::Ok(())
        })??;

        db.view(|rtx| -> eyre::Result<()> {
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

        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create one data root with two chunks and mark them old to be eligible
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                data_root: DataRoot::random(),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
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

            // Mark both chunks as very old
            let mut cur = wtx.cursor_dup_write::<CachedChunksIndex>()?;
            let mut walk = cur.walk_dup(Some(tx_header.data_root), None)?;
            while let Some((_k, entry)) = walk.next().transpose()? {
                wtx.delete::<CachedChunksIndex>(tx_header.data_root, Some(entry.clone()))?;
                let mut updated = entry.clone();
                updated.meta.updated_at = UnixTimestamp::from_secs(0);
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
        };
        task_below.prune_cache(0)?;

        db.view(|rtx| -> eyre::Result<()> {
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

        // Above-capacity prune: set max to 64B so 64B cache > 51.2B threshold
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
        };
        task_above.prune_cache(0)?;

        db.view(|rtx| -> eyre::Result<()> {
            // Expect indices pruned now that we're above capacity
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

    // Chunks should not be pruned when proof generation state marks the data_root active, even with no ingress proof yet.
    #[tokio::test]
    async fn skips_pruning_during_active_generation_state() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;
        let chunk = UnpackedChunk {
            data_root: tx_header.data_root,
            data_size: tx_header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(vec![3_u8; 8]),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        db.update(|wtx| {
            database::cache_chunk(wtx, &chunk)?;
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
        let generation_state = IngressProofGenerationState::new();
        generation_state.mark_generating(tx_header.data_root);
        let service_task = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: generation_state,
            cache_sender: tx,
        };

        service_task.prune_chunks_without_active_ingress_proofs()?;

        db.view(|rtx| -> eyre::Result<()> {
            let mut dup_cursor = rtx.cursor_dup_read::<CachedChunksIndex>()?;
            let mut walk = dup_cursor.walk(Some(tx_header.data_root))?;
            let still_present = walk.next().transpose()?.is_some();
            eyre::ensure!(
                still_present,
                "CachedChunksIndex entry wrongly pruned during active generation state"
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;

        // Set expiry_height to 5 so prune_height > expiry would trigger deletion
        db.update(|wtx| {
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

        db.update(|wtx| {
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
        };

        // Prune with prune_height greater than expiry (6 > 5).
        // Normally this would delete the data root, but because of the local proof, it should stay.
        service_task.prune_data_root_cache(6)?;

        // Verify it was NOT pruned
        db.view(|rtx| -> eyre::Result<()> {
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
    /// `expiry_height`, no local ingress proof.  Under the lifetime model
    /// this entry has no remaining tenants and no bound — evict on next
    /// prune cycle.
    ///
    /// Note: the prior version of this test exercised a CDR with a pending
    /// tx in `txid_set` and asserted eviction.  That semantics was changed
    /// when the pending-tx guard was added to protect cross-ledger
    /// same-`data_root` sibling txs from premature prune; pending txs now
    /// keep the CDR alive (see `does_not_prune_cdr_with_pending_sibling_tx`).
    /// The truly-anomalous arm now only fires when `txid_set` is empty.
    #[tokio::test]
    async fn prunes_truly_anomalous_empty_txid_set() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
        db.update(|wtx| -> eyre::Result<()> {
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
        };

        service_task.prune_data_root_cache(1)?;

        db.view(|rtx| -> eyre::Result<()> {
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
        db.update(|wtx| {
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
        db.update(|wtx| {
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
        };

        // Without the local-proof exemption the (None, None) state would be
        // evicted; with it the entry must survive.
        service_task.prune_data_root_cache(1)?;

        db.view(|rtx| -> eyre::Result<()> {
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
            // Both txs land in the same CDR.txid_set via the shared data_root.
            database::cache_data_root(wtx, &confirmed_tx, None)?;
            database::cache_data_root(wtx, &pending_tx, None)?;
            // The confirmed sibling gets an inclusion height (term-ledger
            // confirmation); the pending tx remains without a metadata row.
            irys_database::set_data_tx_included_height(wtx, &confirmed_tx_id, 5)
                .map_err(|e| eyre::eyre!("set_data_tx_included_height: {:?}", e))?;
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
        };

        // `prune_height = 100` is well past the confirmed tx's `included_height`
        // = 5.  Without the pending-tx guard the CDR would be evicted on
        // inclusion alone.
        service_task.prune_data_root_cache(100)?;

        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(data_root)?.is_some();
            eyre::ensure!(
                has_root,
                "CDR with pending sibling tx (no included_height) must NOT be pruned"
            );
            Ok(())
        })??;

        Ok(())
    }

    // Verifies that `PruneBlockHashFromBlockSet` removes a stale block hash from
    // `CachedDataRoot.block_set` and leaves other hashes untouched.
    //
    // NOTE: This exercises an intentionally parked primitive — no live
    // production emitter sends `PruneBlockHashFromBlockSet` after the
    // `AssignedProofBlockMissing` recovery walk was deleted (commit
    // 7da520194). The test is kept so the primitive stays wirable for
    // future stale-entry observers. See the action's doc-comment.
    #[tokio::test]
    async fn prune_block_hash_from_block_set_removes_stale_hash() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let _temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Build a transaction header so we have a real data_root to key on.
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let data_root = tx_header.data_root;

        // Persist a CachedDataRoot with two block hashes.
        let stale_hash = H256::random();
        let good_hash = H256::random();
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            // Manually add two hashes to block_set.
            let mut cached = cached_data_root_by_data_root(wtx, data_root)?.expect("just inserted");
            cached.block_set.push(stale_hash);
            cached.block_set.push(good_hash);
            wtx.put::<CachedDataRoots>(data_root, cached)?;
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

        let inner = InnerCacheTask {
            db: db.clone(),
            block_tree_guard,
            block_index_guard,
            config,
            gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
            ingress_proof_generation_state: IngressProofGenerationState::new(),
            cache_sender: tx,
        };

        // Spawn the prune task and wait for the background thread to finish.
        inner.spawn_prune_block_hash_task(data_root, stale_hash);

        // Give the background thread time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        db.view(|rtx| -> eyre::Result<()> {
            let cached = rtx
                .get::<CachedDataRoots>(data_root)?
                .expect("CachedDataRoot should still exist");
            assert!(
                !cached.block_set.contains(&stale_hash),
                "stale block hash should have been removed"
            );
            assert!(
                cached.block_set.contains(&good_hash),
                "good block hash should remain"
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
            // CachedDataRoot with one pending txid (any_unpromoted = true).
            let cdr = CachedDataRoot {
                data_size: 64,
                data_size_confirmed: false,
                txid_set: vec![txid],
                block_set: vec![],
                expiry_height: None,
                cached_at: irys_types::UnixTimestamp::now()?,
            };
            wtx.put::<CachedDataRoots>(data_root, cdr)?;

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
                wtx,
                &make_proof(valid_anchor),
                addr_a,
            )?;
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &make_proof(valid_anchor),
                addr_b,
            )?;
            // One expired proof (unknown anchor).
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &make_proof(invalid_anchor),
                addr_c,
            )?;

            Ok(())
        })??;

        // Sanity: three proofs inserted.
        let initial_count = db.view_eyre(|rtx| {
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
        };

        service_task.prune_ingress_proofs()?;

        db.view(|rtx| -> eyre::Result<()> {
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

    /// Regression: when the cache is at capacity, a locally-produced expired
    /// proof with unpromoted txs must be deleted (not regenerated), and the
    /// content-checked delete path (`delete_ingress_proof_if_unchanged`) must
    /// be used so a concurrently-refreshed proof is preserved.
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
            // CachedDataRoot with one pending (unpromoted) txid.
            let cdr = irys_database::db_cache::CachedDataRoot {
                data_size: 64,
                data_size_confirmed: false,
                txid_set: vec![txid],
                block_set: vec![],
                expiry_height: None,
                cached_at: irys_types::UnixTimestamp::now()?,
            };
            wtx.put::<CachedDataRoots>(data_root, cdr)?;

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
            irys_database::store_external_ingress_proof_checked(wtx, &local_proof, local_addr)?;

            // Sibling proof with valid anchor → not expired → survives.
            let mut sibling_proof = IngressProof::default();
            sibling_proof.data_root = data_root;
            sibling_proof.anchor = valid_anchor;
            irys_database::store_external_ingress_proof_checked(wtx, &sibling_proof, sibling_addr)?;

            Ok(())
        })??;

        let initial_count = db.view_eyre(|rtx| {
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
        };

        service_task.prune_ingress_proofs()?;

        db.view(|rtx| -> eyre::Result<()> {
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
        let db_env = open_or_create_db(
            &_temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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
            // CDR with one pending (unpromoted) txid.
            let cdr = CachedDataRoot {
                data_size: 64,
                data_size_confirmed: false,
                txid_set: vec![txid],
                block_set: vec![],
                expiry_height: None,
                cached_at: irys_types::UnixTimestamp::now()?,
            };
            wtx.put::<CachedDataRoots>(data_root, cdr)?;

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
                wtx,
                &make_proof(valid_anchor),
                addr_a,
            )?;
            // addr_b and addr_c: expired anchors → will be pruned.
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &make_proof(expired_anchor_b),
                addr_b,
            )?;
            irys_database::store_external_ingress_proof_checked(
                wtx,
                &make_proof(expired_anchor_c),
                addr_c,
            )?;

            Ok(())
        })??;

        // Sanity: three proofs inserted.
        let initial_count = db.view_eyre(|rtx| {
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
        };

        service_task.prune_ingress_proofs()?;

        db.view(|rtx| -> eyre::Result<()> {
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
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        let data_root = H256::random();
        let cdr = CachedDataRoot {
            data_size: 64,
            data_size_confirmed: false,
            txid_set: txids,
            block_set: vec![],
            expiry_height: Some(100),
            cached_at: irys_types::UnixTimestamp::now()?,
        };
        db.update(|wtx| -> eyre::Result<()> {
            wtx.put::<CachedDataRoots>(data_root, cdr)?;
            Ok(())
        })??;
        Ok((temp_dir, db, config, data_root))
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

        db.view(|rtx| -> eyre::Result<()> {
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

        db.view(|rtx| -> eyre::Result<()> {
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

        db.view(|rtx| -> eyre::Result<()> {
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

    // T4 (regression pin for the writer-lock-wait race): a tx (re-)confirmed
    // in the canonical tree while the scrub is parked on the MDBX writer lock
    // must be kept. The tree snapshot must be read inside the write txn (after
    // the lock is held), so a confirmation landing during the wait is observed;
    // a snapshot taken before `update_eyre` would freeze the tree ahead of the
    // wait and wrongly scrub the tx. The test holds the global writer lock, so
    // the scrub's in-closure tree read is ordered strictly after the tree
    // mutation below — no sleeps, deterministic.
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

        // Hold the global MDBX writer lock so the scrub blocks inside
        // `update_eyre` before it can read the tree.
        let blocker = db.tx_mut()?;

        // Queue tx_x for removal and run the scrub on another thread; with the
        // fix it parks on the writer lock this thread holds, ahead of the tree read.
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

        // Release the writer lock; the scrub proceeds and reads the tree (now
        // carrying tx_x) inside its own write txn.
        drop(blocker);

        handle.join().expect("prune thread panicked")?;

        db.view(|rtx| -> eyre::Result<()> {
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

    // T5 (deadlock guard): a single scrub attempt must be NON-BLOCKING on the
    // tree. While the tree lock is write-held (as `on_block_validation_finished`
    // does across its deep-reorg branch, which itself opens a consensus-DB write
    // txn), the attempt must report Deferred with nothing written — not block on
    // the tree while holding the MDBX writer lock, which would complete an AB-BA
    // deadlock. Holding the write guard on THIS thread makes `try_read` fail
    // deterministically without any deadlock.
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
        db.view(|rtx| -> eyre::Result<()> {
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
        db.view(|rtx| -> eyre::Result<()> {
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

    // T6 (batch survives contention): the retrying wrapper must carry a
    // deferred one-shot batch through to completion once the tree lock frees —
    // dropping it would leave stale `txid_set` entries with no later producer
    // to re-send them. The worker thread parks on the retry loop while this
    // thread holds the tree write lock, then completes after the drop.
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

        db.view(|rtx| -> eyre::Result<()> {
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
