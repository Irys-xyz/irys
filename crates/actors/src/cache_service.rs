use crate::chunk_ingress_service::ChunkIngressServiceInner;
use crate::chunk_ingress_service::ingress_proofs::{
    RegenAction, generate_and_store_ingress_proof, reanchor_and_store_ingress_proof,
};
use crate::metrics;
use irys_database::{
    cached_data_root_by_data_root, delete_cached_chunks_by_data_root_older_than,
    get_data_tx_metadata, tx_header_by_txid,
};
use irys_database::{
    db::IrysDatabaseExt as _,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{CachedChunks, CachedDataRoots, IngressProofs},
};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::ingress::CachedIngressProof;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    Config, DataLedger, DataRoot, DatabaseProvider, GIGABYTE, H256, IngressProof, IrysAddress,
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
        if let Some(latest) = self.block_index_guard.read().get_latest_item() {
            let submit_ledger_max_chunk_offset = latest.ledgers[ledger_id].total_chunks;
            if submit_ledger_max_chunk_offset > chunk_offset {
                let block_bounds = self
                    .block_index_guard
                    .read()
                    .get_block_bounds(ledger_id, LedgerChunkOffset::from(chunk_offset))
                    .expect("Should be able to get block bounds as max_chunk_offset was checked");
                // Genesis block (height 0) never enters block_index as it has no submit ledger
                // data, use saturating_sub (defensive).
                prune_height = Some(block_bounds.height.saturating_sub(1));
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
                // ingress (`cache_data_root_with_expiry`), or (b) `included_height`
                // set atomically with the confirming tip change in
                // `BlockMigrationService::persist_metadata` (Phase 2 sets
                // `included_height`, Phase 3 clears `expiry_height` — same write
                // tx).  The local-proof exemption below extends (b) indefinitely
                // while a local ingress proof is present.
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
    /// destroying chunks that are still needed.  Per-txid metadata recheck
    /// inside the write tx closes that race: if `IrysDataTxMetadata[tx_id]`
    /// is `Some` now, the tx is confirmed and the txid_set entry must stay.
    ///
    /// Residual race the recheck does not close: a peer gossips the tx back
    /// in during the scrub window and `cache_data_root` re-adds it to
    /// `txid_set`, but the tx has not yet confirmed (no metadata row).  The
    /// scrub would drop it.  Bounded by mempool TTL re-population and
    /// `cache_data_root` idempotency on the next ingress.  Acceptable for
    /// the current architecture; revisit if the mempool grows a strong
    /// "currently-pending" signal that can be consulted from MDBX context.
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
            let result: eyre::Result<()> = match std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| {
                    clone.db.update_eyre(|db_tx| {
                for (data_root, txids_to_remove) in &by_data_root {
                    let Some(mut cached) = cached_data_root_by_data_root(db_tx, *data_root)? else {
                        continue;
                    };

                    // Build the actually-safe-to-remove set: any tx_id whose
                    // metadata row exists now was (re-)confirmed since the
                    // scrub was queued and must stay in txid_set — see the
                    // race rationale in the function docs above.
                    let mut removal_set: HashSet<H256> = HashSet::new();
                    let mut raced_confirmed: usize = 0;
                    for tx_id in txids_to_remove {
                        if get_data_tx_metadata(db_tx, tx_id)?.is_some() {
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
                Ok(())
            })
                }),
            ) {
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
        let mut to_delete: Vec<(DataRoot, IrysAddress)> = Vec::new();
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
            let CachedIngressProof { address, proof } = compact.0;

            // Associated txids
            let Some(cached_data_root) = cached_data_root_by_data_root(&tx, data_root)? else {
                debug!(ingress_proof.data_root = ?data_root, "Proof has no cached data root; marking for deletion");
                to_delete.push((data_root, address));
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
                to_delete.push((data_root, address));
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
                to_delete.push((data_root, address));
                debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = false, "Marking expired proof for deletion (promoted)");
            }
        }

        // Delete expired proofs — per-signer to avoid wiping other signers' rows on the same data_root
        if !to_delete.is_empty() {
            for (root, addr) in to_delete.iter() {
                if let Err(e) =
                    ChunkIngressServiceInner::remove_ingress_proof_by_signer(&self.db, *root, *addr)
                {
                    warn!(ingress_proof.data_root = ?root, "Failed to remove ingress proof: {e}");
                }
            }
            info!(
                proofs.deleted = to_delete.len(),
                "Deleted expired ingress proofs"
            );
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
                if let Err(e) = ChunkIngressServiceInner::remove_ingress_proof_by_signer(
                    &self.db,
                    proof.data_root,
                    local_addr,
                ) {
                    warn!(ingress_proof.data_root = ?proof, "Failed to remove ingress proof: {e}");
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
                if let Err(e) = ChunkIngressServiceInner::remove_ingress_proof_by_signer(
                    &self.db,
                    proof.data_root,
                    local_addr,
                ) {
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
    use irys_testing_utils::{initialize_tracing, new_mock_signed_header};
    use irys_types::{
        Base64, Config, DataTransactionHeader, DataTransactionHeaderV1,
        DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, NodeConfig, TxChunkOffset,
        UnpackedChunk, app_state::DatabaseProvider,
    };
    use reth_db::cursor::DbDupCursorRO as _;
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::{Arc, RwLock};

    // This test prevents a regression of bug: mempool-only data roots (with empty block_set field)
    // are pruned once prune_height > 0 and they should not be pruned!
    //
    // Real prod ingress goes through `cache_data_root_with_expiry`, which sets
    // `expiry_height` to `anchor + tx_anchor_expiry_depth`.  Direct callers of
    // `cache_data_root(_, _, None)` (this fixture, pre-fix code paths) leave
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
}
