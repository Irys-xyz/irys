use crate::chunk_ingress_service::ChunkIngressServiceInner;
use crate::chunk_ingress_service::ingress_proofs::{
    RegenAction, generate_and_store_ingress_proof, reanchor_and_store_ingress_proof,
};
use crate::metrics;
use irys_database::{
    cached_data_root_by_data_root, delete_cached_chunks_by_data_root_older_than, tx_header_by_txid,
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
    Config, DataLedger, DataRoot, DatabaseProvider, GIGABYTE, IngressProof, LedgerChunkOffset,
    SendTraced as _, TokioServiceHandle, Traced, UnixTimestamp,
};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{Instrument as _, debug, error, info, trace, warn};

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
        let signer = self.config.irys_signer();
        let local_addr = signer.address();
        let mut chunks_pruned: u64 = 0;
        let mut eviction_count: usize = 0;
        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<CachedDataRoots>()?;
        let mut walker = cursor.walk(None)?;
        while let Some((data_root, cached)) = walker.next().transpose()? {
            if eviction_count >= MAX_EVICTIONS_PER_RUN {
                warn!(
                    evictions_performed = eviction_count,
                    "Hit max eviction limit in prune_data_root_cache, will continue next cycle"
                );
                break;
            }
            // Pruning horizon priority: block inclusion > expiry height > skip
            // Rationale: Confirmed blocks provide the most reliable pruning point.
            // Unconfirmed mempool entries use expiry_height as a conservative fallback.
            // Entries without either metadata are likely still active and should not be pruned.
            let mut inclusion_max_height: Option<u64> = None;
            for block_hash in cached.block_set.iter() {
                if let Some(block_header) =
                    irys_database::block_header_by_hash(&write_tx, block_hash, false)?
                {
                    inclusion_max_height = Some(
                        inclusion_max_height
                            .map_or(block_header.height, |h| h.max(block_header.height)),
                    );
                }
            }
            let horizon = match (inclusion_max_height, cached.expiry_height) {
                (Some(h), _) => Some(h),
                (None, Some(e)) => Some(e),
                (None, None) => None,
            };
            let max_height: u64 = match horizon {
                Some(h) => h,
                None => {
                    trace!(
                        data_root.data_root = ?data_root,
                        "Skipping prune for data root without inclusion or expiry"
                    );
                    continue;
                }
            };

            trace!(
                "Processing data root {} max height: {}, prune height: {}",
                &data_root, &max_height, &prune_height
            );

            if max_height < prune_height {
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
                        "Skipping prune for data root with locally generated ingress proof"
                    );
                    continue;
                }

                debug!(
                    data_root.data_root = ?data_root,
                    data_root.max_height = ?max_height,
                    data_root.prune_height = ?prune_height,
                    "expiring cached data for data root",
                );
                write_tx.delete::<IngressProofs>(data_root, None)?;
                chunks_pruned = chunks_pruned
                    .saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
                write_tx.delete::<CachedDataRoots>(data_root, None)?;
                eviction_count += 1;
            }
        }
        debug!(data_root.chunks_pruned = ?chunks_pruned, "Pruned chunks");
        write_tx.commit()?;

        Ok(())
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
        let mut to_delete: Vec<DataRoot> = Vec::new();
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
                to_delete.push(data_root);
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
                if let Some(tx_header) = tx_header_by_txid(&tx, txid)? {
                    if tx_header.promoted_height().is_none() {
                        any_unpromoted = true;
                        debug!(ingress_proof.data_root = ?data_root, tx.id = ?tx_header.id, "Found unpromoted tx for data root");
                        break;
                    }
                }
            }

            let is_locally_produced = address == local_addr;

            if at_capacity {
                // Unpromoted + expired + at capacity: delete
                to_delete.push(data_root);
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
                to_delete.push(data_root);
                debug!(ingress_proof.data_root = ?data_root, cache.at_capacity = false, "Marking expired proof for deletion (promoted)");
            }
        }

        // Delete expired proofs using a proper removal function
        if !to_delete.is_empty() {
            for root in to_delete.iter() {
                if let Err(e) = ChunkIngressServiceInner::remove_ingress_proof(&self.db, *root) {
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
                if let Err(e) =
                    ChunkIngressServiceInner::remove_ingress_proof(&self.db, proof.data_root)
                {
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
                if let Err(e) =
                    ChunkIngressServiceInner::remove_ingress_proof(&self.db, proof.data_root)
                {
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
            let res = clone.prune_cache(migration_height);
            let completion = match &res {
                Ok(_) => Ok(()),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            };
            if let Some(sender) = response_sender {
                if let Err(error) = sender.send(res) {
                    warn!(custom.error = ?error, "RX failure for OnBlockMigrated");
                }
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
            let res = clone.on_epoch_processed(epoch_snapshot);
            let completion = match &res {
                Ok(_) => Ok(()),
                Err(e) => Err(eyre::eyre!(e.to_string())),
            };
            if let Some(sender) = response_sender {
                if let Err(e) = sender.send(res) {
                    warn!(custom.error = ?e, "Unable to send a response for OnEpochProcessed")
                }
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

    #[tracing::instrument(level = "trace", skip_all)]
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
                if !self.pruning_running {
                    if let Some((h, s)) = self.pruning_queue.pop_front() {
                        self.pruning_running = true;
                        self.cache_task.spawn_pruning_task(h, s);
                    }
                }
            }
            CacheServiceAction::OnEpochProcessed(epoch_snapshot, sender) => {
                // Enqueue epoch processing; start if idle
                self.epoch_queue.push_back((epoch_snapshot, sender));
                if !self.epoch_running {
                    if let Some((e, s)) = self.epoch_queue.pop_front() {
                        self.epoch_running = true;
                        self.cache_task.spawn_epoch_processing(e, s);
                    }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_database::{
        database, open_or_create_db,
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
    use std::sync::{Arc, RwLock};

    // This test prevents a regression of bug: mempool-only data roots (with empty block_set field)
    // are pruned once prune_height > 0 and they should not be pruned!
    #[tokio::test]
    async fn does_not_prune_unconfirmed_data_roots() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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

    // ========================================================================
    // FIFO Ordering Property Tests
    // ========================================================================

    #[cfg(test)]
    mod fifo_properties {

        use irys_types::UnixTimestamp;
        use proptest::prelude::*;

        prop_compose! {
            fn unix_timestamps()(
                // Realistic range: 2020-01-01 to 2030-01-01
                secs in 1_577_836_800_u64..1_893_456_000_u64
            ) -> UnixTimestamp {
                UnixTimestamp::from_secs(secs)
            }
        }
    }

    // Chunks should remain when there is an active ingress proof present for the data_root.
    #[tokio::test]
    async fn does_not_prune_chunks_with_active_proof() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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

        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
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
}
