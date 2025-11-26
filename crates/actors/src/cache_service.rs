use std::collections::HashSet;
use crate::ingress_proofs::{
    generate_and_store_ingress_proof, reanchor_and_store_ingress_proof, RegenAction,
};
use crate::mempool_service::Inner;
use irys_database::{cached_data_root_by_data_root, tx_header_by_txid};
use irys_database::{
    db::IrysDatabaseExt as _,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{CachedChunks, CachedDataRoots, IngressProofs},
};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::ingress::CachedIngressProof;
use irys_types::{
    Config, DataLedger, DataRoot, DatabaseProvider, GossipBroadcastMessage, IngressProof,
    LedgerChunkOffset, TokioServiceHandle, GIGABYTE,
};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use std::sync::{Arc, RwLock};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{debug, error, info, warn, Instrument as _};

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
    /// Marks the start of ingress proof generation for the specified data root. Chunks that are
    /// related to this data root should not be pruned if the ingress proof is still being generated.
    NotifyProofGenerationStarted(DataRoot),
    /// Send this when ingress proof generation is completed and the proof has been persisted to the
    /// db. Chunks related to this data root can now be pruned if needed.
    NotifyProofGenerationCompleted(DataRoot),
}

/// Tracks data roots for which ingress proofs are currently being generated
/// to prevent race conditions with chunk pruning
#[derive(Clone, Debug)]
pub struct IngressProofGenerationState {
    inner: Arc<RwLock<HashSet<DataRoot>>>,
}

impl IngressProofGenerationState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn mark_generating(&self, data_root: DataRoot) -> bool {
        self.inner.write().expect("expected to acquire a lock for an ingress proof generation state").insert(data_root)
    }

    pub fn unmark_generating(&self, data_root: DataRoot) {
        self.inner.write().expect("expected to acquire a lock for an ingress proof generation state").remove(&data_root);
    }

    pub fn is_generating(&self, data_root: DataRoot) -> bool {
        self.inner.read().expect("expected to acquire a lock for an ingress proof generation state").contains(&data_root)
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
    pub gossip_broadcast: UnboundedSender<GossipBroadcastMessage>,
    pub ingress_proof_generation_state: IngressProofGenerationState,
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
                let block_hash = block_entry.block_hash;
                let block_tree = self.block_tree_guard.read();
                let block = block_tree.get_block(&block_hash)?;
                let ledger_total_chunks = block.data_ledgers[ledger_id].total_chunks;
                if ledger_total_chunks <= chunk_offset {
                    Some((block_entry.height, ledger_total_chunks))
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

        let max_cache_size_bytes = self.config.node_config.cache.max_cache_size_bytes;
        let size_limit_exceeded = chunk_cache_size > max_cache_size_bytes;

        if size_limit_exceeded {
            info!(
                custom.size_exceeded = size_limit_exceeded,
                custom.current_size_gb = (chunk_cache_size / GIGABYTE as u64),
                custom.max_size_gb = (max_cache_size_bytes / GIGABYTE as u64),
                custom.current_count = chunk_cache_count,
                "Cache limit exceeded"
            );

            // DISABLED. cache pruning logic NEEDS to respect the lifecycle rules for cached data roots and their associated chunks.
            // self.prune_cache_by_size(chunk_cache_count, chunk_cache_size, *max_cache_size_bytes)?;
        } else {
            debug!("Cache within size limits, no eviction needed");
        }

        // Proof expiry/regeneration (single authority)
        self.prune_ingress_proofs()?;
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(prune_height))]
    fn prune_data_root_cache(&self, prune_height: u64) -> eyre::Result<()> {
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
                    debug!(
                        data_root.data_root = ?data_root,
                        "Skipping prune for data root without inclusion or expiry"
                    );
                    continue;
                }
            };

            debug!(
                "Processing data root {} max height: {}, prune height: {}",
                &data_root, &max_height, &prune_height
            );

            if max_height < prune_height {
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
            let check_result = Inner::is_ingress_proof_expired_static(
                &self.block_tree_guard,
                &self.db,
                &self.config,
                &proof,
            );
            if !check_result.expired_or_invalid {
                continue;
            }
            // Associated txids
            let Some(cached_data_root) = cached_data_root_by_data_root(&tx, data_root)? else {
                debug!(ingress_proof.data_root = ?data_root, "Expired proof has no cached data root; skipping actions");
                continue;
            };
            let mut any_unpromoted = false;
            for txid in cached_data_root.txid_set.iter() {
                if let Some(tx_header) = tx_header_by_txid(&tx, txid)? {
                    if tx_header.promoted_height.is_none() {
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
                        error!("We're under capacity, and the proof is expired and local with unpromoted txs, but proof with data root {} does not meet reanchoring or regeneration criteria. This should not happen.", &data_root);
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
                if let Err(e) = Inner::remove_ingress_proof(&self.db, *root) {
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
                if let Err(e) = reanchor_and_store_ingress_proof(
                    &self.block_tree_guard,
                    &self.db,
                    &self.config,
                    &signer,
                    proof,
                    &self.gossip_broadcast,
                ) {
                    warn!(ingress_proof.data_root = ?proof, "Failed to regenerate ingress proof: {e}");
                }
            } else {
                debug!(
                    ingress_proof.data_root = ?proof.data_root,
                    "Skipping reanchoring of ingress proof due to REGENERATE_PROOFS = false"
                );
                if let Err(e) = Inner::remove_ingress_proof(&self.db, proof.data_root) {
                    warn!(ingress_proof.data_root = ?proof, "Failed to remove ingress proof: {e}");
                }
            }
        }

        for proof in to_regen.iter() {
            if REGENERATE_PROOFS {
                if let Err(report) = generate_and_store_ingress_proof(
                    &self.block_tree_guard,
                    &self.db,
                    &self.config,
                    proof.data_root,
                    None,
                    &self.gossip_broadcast,
                ) {
                    warn!(ingress_proof.data_root = ?proof.data_root, "Failed to regenerate ingress proof: {report}");
                }
            } else {
                debug!(
                    ingress_proof.data_root = ?proof.data_root,
                    "Regeneration disabled, removing ingress proof for data root"
                );
                if let Err(e) = Inner::remove_ingress_proof(&self.db, proof.data_root) {
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

    fn spawn_pruning_task(&self, migration_height: u64, response_sender: Option<oneshot::Sender<eyre::Result<()>>>) {
        let clone = self.clone();
        std::thread::spawn(move || {
            let res = clone.prune_cache(migration_height);
            let Some(sender) = response_sender else { return };
            if let Err(error) = sender.send(res) {
                warn!(custom.error = ?error, "RX failure for OnBlockMigrated");
            }
        });
    }

    fn spawn_epoch_processing(&self, epoch_snapshot: Arc<EpochSnapshot>, response_sender: Option<oneshot::Sender<eyre::Result<()>>>) {
        let clone = self.clone();
        std::thread::spawn(move || {
            let res = clone.on_epoch_processed(epoch_snapshot);
            if let Some(sender) = response_sender {
                if let Err(e) = sender.send(res) {
                    warn!(custom.error = ?e, "Unable to send a response for OnEpochProcessed")
                }
            }
        });
    }
}

pub type CacheServiceSender = UnboundedSender<CacheServiceAction>;

#[derive(Debug)]
pub struct ChunkCacheService {
    pub msg_rx: UnboundedReceiver<CacheServiceAction>,
    pub shutdown: Shutdown,
    pub cache_task: InnerCacheTask
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
        rx: UnboundedReceiver<CacheServiceAction>,
        config: Config,
        gossip_broadcast: UnboundedSender<GossipBroadcastMessage>,
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
                }
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
                        Some(msg) => {
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
        while let Ok(msg) = self.msg_rx.try_recv() {
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
    #[tracing::instrument(level = "trace", skip_all)]
    fn on_handle_message(&mut self, msg: CacheServiceAction) {
        match msg {
            CacheServiceAction::OnBlockMigrated(migration_height, sender) => {
                self.cache_task.spawn_pruning_task(migration_height, sender);
            }
            CacheServiceAction::OnEpochProcessed(epoch_snapshot, sender) => {
                self.cache_task.spawn_epoch_processing(epoch_snapshot, sender);
            }
            CacheServiceAction::NotifyProofGenerationStarted(data_root) => {
                self.cache_task.ingress_proof_generation_state.mark_generating(data_root);
            }
            CacheServiceAction::NotifyProofGenerationCompleted(data_root) => {
                self.cache_task.ingress_proof_generation_state.unmark_generating(data_root);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::WrapErr as _;
    use irys_database::{
        database, open_or_create_db,
        tables::{CachedDataRoots, IrysTables},
    };
    use irys_domain::{BlockIndex, BlockTree};
    use irys_types::{
        app_state::DatabaseProvider, Base64, Config, DataTransactionHeader,
        DataTransactionHeaderV1, IrysBlockHeader, NodeConfig, TxChunkOffset, UnpackedChunk,
    };
    use std::sync::{Arc, RwLock};

    // This test prevents a regression of bug: mempool-only data roots (with empty block_set field)
    // are pruned once prune_height > 0 and they should not be pruned!
    #[tokio::test]
    async fn does_not_prune_unconfirmed_data_roots() -> eyre::Result<()> {
        let node_config = NodeConfig::testing();
        let config = Config::new(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1 {
            data_size: 64,
            ..Default::default()
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

        let genesis_block = IrysBlockHeader::new_mock_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new(&config.node_config)
            .await
            .wrap_err("failed to build BlockIndex for test")?;
        let block_index_guard = irys_domain::block_index_guard::BlockIndexReadGuard::new(Arc::new(
            RwLock::new(block_index),
        ));

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let service = ChunkCacheService {
            msg_rx: rx,
            shutdown: shutdown_rx,
            cache_task: InnerCacheTask {
                db: db.clone(),
                block_tree_guard,
                block_index_guard,
                config: config.clone(),
                gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
                ingress_proof_generation_state: IngressProofGenerationState::new(),
            }
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
        let config = Config::new(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1 {
            data_size: 64,
            ..Default::default()
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

        let genesis_block = IrysBlockHeader::new_mock_header();
        let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let block_index = BlockIndex::new(&config.node_config)
            .await
            .wrap_err("failed to build BlockIndex for test")?;
        let block_index_guard = irys_domain::block_index_guard::BlockIndexReadGuard::new(Arc::new(
            RwLock::new(block_index),
        ));

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let service = ChunkCacheService {
            msg_rx: rx,
            shutdown: shutdown_rx,
            cache_task: InnerCacheTask {
                db: db.clone(),
                block_tree_guard,
                block_index_guard,
                config: config.clone(),
                gossip_broadcast: tokio::sync::mpsc::unbounded_channel().0,
                ingress_proof_generation_state: IngressProofGenerationState::new(),
            }
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
}
