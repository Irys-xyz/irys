use irys_database::{
    db::IrysDatabaseExt as _,
    db_cache::CachedDataRoot,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{CachedChunks, CachedDataRoots, IngressProofs},
};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::{
    CacheEvictionStrategy, Config, DataLedger, DataRoot, DatabaseProvider, LedgerChunkOffset,
    TokioServiceHandle, GIGABYTE,
};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use std::sync::Arc;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{debug, info, warn, Instrument as _};

/// Maximum evictions per invocation to prevent blocking the service.
/// If this limit is reached, eviction will continue in the next cycle.
const MAX_EVICTIONS_PER_RUN: usize = 10_000;

/// Eviction target to prevent thrashing near size limit boundary.
/// Evicts to 90% (9/10) of limit to provide a buffer before next eviction.
/// Example: 10GB limit evicts down to 9GB, providing 1GB buffer.
const SIZE_EVICTION_TARGET_NUMERATOR: u64 = 9;
const SIZE_EVICTION_TARGET_DENOMINATOR: u64 = 10;

#[derive(Debug)]
pub enum CacheServiceAction {
    OnBlockMigrated(u64, Option<oneshot::Sender<eyre::Result<()>>>),
    OnEpochProcessed(
        Arc<EpochSnapshot>,
        Option<oneshot::Sender<eyre::Result<()>>>,
    ),
}

pub type CacheServiceSender = UnboundedSender<CacheServiceAction>;

#[derive(Debug)]
pub struct ChunkCacheService {
    pub config: Config,
    pub block_index_guard: BlockIndexReadGuard,
    pub block_tree_guard: BlockTreeReadGuard,
    pub db: DatabaseProvider,
    pub msg_rx: UnboundedReceiver<CacheServiceAction>,
    pub shutdown: Shutdown,
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
    pub fn spawn_service(
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        db: DatabaseProvider,
        rx: UnboundedReceiver<CacheServiceAction>,
        config: Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning chunk cache service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(async move {
            let cache_service = Self {
                shutdown: shutdown_rx,
                block_index_guard,
                block_tree_guard,
                db,
                config,
                msg_rx: rx,
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

        debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
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
    fn on_handle_message(&mut self, msg: CacheServiceAction) {
        match msg {
            CacheServiceAction::OnBlockMigrated(migration_height, sender) => {
                let res = self.prune_cache(migration_height);
                let Some(sender) = sender else { return };
                if let Err(error) = sender.send(res) {
                    warn!(?error, "RX failure for OnBlockMigrated");
                }
            }
            CacheServiceAction::OnEpochProcessed(epoch_snapshot, sender) => {
                let res = self.on_epoch_processed(epoch_snapshot);
                if let Some(sender) = sender {
                    if let Err(e) = sender.send(res) {
                        warn!(?e, "Unable to send response for OnEpochProcessed")
                    }
                }
            }
        }
    }

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
            ?migration_height,
            "Chunk cache: {} chunks ({:.3} GB),  {} ingress proofs",
            chunk_cache_count,
            (chunk_cache_size / GIGABYTE as u64),
            ingress_proof_count
        );

        match &self.config.node_config.cache.eviction_strategy {
            CacheEvictionStrategy::TimeBased { max_age_seconds } => {
                debug!(max_age_seconds, "Running time-based cache eviction");
                self.prune_cache_by_time(*max_age_seconds)?;
            }
            CacheEvictionStrategy::SizeBased {
                max_cache_size_bytes,
            } => {
                let size_limit_exceeded = chunk_cache_size > *max_cache_size_bytes;

                if size_limit_exceeded {
                    info!(
                        size_exceeded = size_limit_exceeded,
                        current_size_gb = (chunk_cache_size / GIGABYTE as u64),
                        max_size_gb = (max_cache_size_bytes / GIGABYTE as u64),
                        current_count = chunk_cache_count,
                        "Cache limit exceeded, performing size-based eviction (FIFO)"
                    );

                    self.prune_cache_by_size(
                        chunk_cache_count,
                        chunk_cache_size,
                        *max_cache_size_bytes,
                    )?;
                } else {
                    debug!("Cache within size limits, no eviction needed");
                }
            }
        }

        Ok(())
    }

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
                        ?data_root,
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
                    ?data_root,
                    ?max_height,
                    ?prune_height,
                    "expiring cached data for data root",
                );
                write_tx.delete::<IngressProofs>(data_root, None)?;
                chunks_pruned = chunks_pruned
                    .saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
                write_tx.delete::<CachedDataRoots>(data_root, None)?;
                eviction_count += 1;
            }
        }
        debug!(?chunks_pruned, "Pruned chunks");
        write_tx.commit()?;

        Ok(())
    }

    /// Collects all cached data roots with their metadata for FIFO eviction
    /// Returns entries sorted by cached_at timestamp (oldest first)
    fn collect_cache_entries_by_age(&self) -> eyre::Result<Vec<(DataRoot, CachedDataRoot)>> {
        let tx = self.db.tx()?;
        let estimated_count = tx.entries::<CachedDataRoots>()?;
        let mut entries = Vec::with_capacity(estimated_count);

        let mut cursor = tx.cursor_read::<CachedDataRoots>()?;
        let mut walker = cursor.walk(None)?;

        while let Some((data_root, cached)) = walker.next().transpose()? {
            entries.push((data_root, cached));
        }

        entries.sort_by_key(|(_, cached)| cached.cached_at);

        Ok(entries)
    }

    /// Prunes cache entries older than max_age_seconds
    /// Uses FIFO eviction based on cached_at timestamp
    fn prune_cache_by_time(&self, max_age_seconds: u64) -> eyre::Result<()> {
        let now = irys_types::UnixTimestamp::now()
            .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?;

        let expiry_threshold = now.saturating_sub_secs(max_age_seconds);

        debug!(
            now = now.as_secs(),
            threshold = expiry_threshold.as_secs(),
            max_age_seconds,
            "Time-based eviction: checking for expired entries"
        );

        let entries = self.collect_cache_entries_by_age()?;

        let mut evicted_count = 0_u64;
        let mut evicted_size = 0_u64;

        for (data_root, cached) in entries.into_iter().take(MAX_EVICTIONS_PER_RUN) {
            if cached.cached_at >= expiry_threshold {
                break;
            }

            let age_seconds = now.saturating_seconds_since(cached.cached_at);
            debug!(
                ?data_root,
                cached_at = cached.cached_at.as_secs(),
                age_seconds,
                "Evicting expired cache entry"
            );

            let chunk_count = cached.data_size.div_ceil(self.config.consensus.chunk_size);
            let approx_size = chunk_count * self.config.consensus.chunk_size;

            // Each entry gets its own transaction for incremental progress.
            // Batching would be faster but risks all-or-nothing failure on large evictions.
            let write_tx = self.db.tx_mut()?;
            write_tx.delete::<IngressProofs>(data_root, None)?;
            let chunks_removed = delete_cached_chunks_by_data_root(&write_tx, data_root)?;
            write_tx.delete::<CachedDataRoots>(data_root, None)?;
            write_tx.commit()?;

            evicted_count += chunks_removed;
            evicted_size += approx_size;
        }

        info!(
            evicted_count,
            evicted_size_gb = (evicted_size / GIGABYTE as u64),
            max_age_seconds,
            "Time-based cache eviction complete"
        );

        Ok(())
    }

    /// Prunes cache entries to bring cache size under configured limit
    /// Uses FIFO eviction based on cached_at timestamp (oldest first)
    fn prune_cache_by_size(
        &self,
        current_chunk_count: u64,
        current_chunk_size: u64,
        max_cache_size_bytes: u64,
    ) -> eyre::Result<()> {
        let target_size_with_margin = max_cache_size_bytes
            .saturating_div(SIZE_EVICTION_TARGET_DENOMINATOR)
            .saturating_mul(SIZE_EVICTION_TARGET_NUMERATOR);

        debug!(
            current_size_gb = (current_chunk_size / GIGABYTE as u64),
            target_size_gb = (max_cache_size_bytes / GIGABYTE as u64),
            target_with_margin_gb = (target_size_with_margin / GIGABYTE as u64),
            "Size-based eviction: cache limit exceeded"
        );

        let entries = self.collect_cache_entries_by_age()?;

        let mut evicted_count = 0_u64;
        let mut evicted_size = 0_u64;
        let mut running_chunk_count = current_chunk_count;
        let mut running_chunk_size = current_chunk_size;

        let now = irys_types::UnixTimestamp::now()
            .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?;

        for (data_root, cached) in entries.into_iter().take(MAX_EVICTIONS_PER_RUN) {
            if running_chunk_size <= target_size_with_margin {
                break;
            }

            let age_seconds = now.saturating_seconds_since(cached.cached_at);

            debug!(
                ?data_root,
                cached_at = cached.cached_at.as_secs(),
                age_seconds,
                data_size = cached.data_size,
                "Evicting oldest cache entry to free space"
            );

            let chunk_count = cached.data_size.div_ceil(self.config.consensus.chunk_size);
            let approx_size = chunk_count * self.config.consensus.chunk_size;

            // Each entry gets its own transaction for incremental progress.
            // Batching would be faster but risks all-or-nothing failure on large evictions.
            let write_tx = self.db.tx_mut()?;
            write_tx.delete::<IngressProofs>(data_root, None)?;
            let chunks_removed = delete_cached_chunks_by_data_root(&write_tx, data_root)?;
            write_tx.delete::<CachedDataRoots>(data_root, None)?;
            write_tx.commit()?;

            evicted_count += chunks_removed;
            evicted_size += approx_size;
            running_chunk_count = running_chunk_count.saturating_sub(chunks_removed);
            running_chunk_size = running_chunk_size.saturating_sub(approx_size);
        }

        info!(
            evicted_count,
            evicted_size_gb = (evicted_size / GIGABYTE as u64),
            remaining_count = running_chunk_count,
            remaining_size_gb = (running_chunk_size / GIGABYTE as u64),
            "Size-based cache eviction complete (FIFO)"
        );

        Ok(())
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
            config: config.clone(),
            block_index_guard,
            block_tree_guard,
            db: db.clone(),
            msg_rx: rx,
            shutdown: shutdown_rx,
        };

        // Invoke pruning with prune_height > 0 which should NOT delete mempool-only roots
        service.prune_data_root_cache(1)?;

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
            config: config.clone(),
            block_index_guard,
            block_tree_guard,
            db: db.clone(),
            msg_rx: rx,
            shutdown: shutdown_rx,
        };

        // Prune with prune_height greater than expiry (6 > 5) -> should delete
        service.prune_data_root_cache(6)?;

        // Verify it was pruned
        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(!has_root, "CachedDataRoots should have been pruned");
            Ok(())
        })??;

        Ok(())
    }

    // ========================================================================
    // Test Helpers
    // ========================================================================

    async fn setup_test_service_with_strategy(
        strategy: irys_types::CacheEvictionStrategy,
    ) -> eyre::Result<ChunkCacheService> {
        let mut node_config = NodeConfig::testing();
        node_config.cache.eviction_strategy = strategy;
        let config = Config::new(node_config);

        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

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

        Ok(ChunkCacheService {
            config: config.clone(),
            block_index_guard,
            block_tree_guard,
            db: db.clone(),
            msg_rx: rx,
            shutdown: shutdown_rx,
        })
    }

    fn insert_entry_with_timestamp(
        service: &ChunkCacheService,
        timestamp: irys_types::UnixTimestamp,
    ) -> eyre::Result<DataRoot> {
        use rand::Rng as _;

        let mut rng = rand::thread_rng();
        let random_bytes: [u8; 32] = rng.gen();
        let data_root = DataRoot::from(random_bytes);

        service.db.update(|wtx| {
            let cached = irys_database::db_cache::CachedDataRoot {
                data_size: 1024, // 1 KB
                txid_set: vec![],
                block_set: vec![],
                expiry_height: None,
                cached_at: timestamp,
            };
            wtx.put::<CachedDataRoots>(data_root, cached)?;
            eyre::Ok(())
        })??;

        Ok(data_root)
    }

    fn get_cache_entry_count(service: &ChunkCacheService) -> eyre::Result<usize> {
        service
            .db
            .view_eyre(|tx| Ok(tx.entries::<CachedDataRoots>()?))
    }

    fn cache_contains(service: &ChunkCacheService, root: DataRoot) -> eyre::Result<bool> {
        service
            .db
            .view_eyre(|tx| Ok(tx.get::<CachedDataRoots>(root)?.is_some()))
    }

    // ========================================================================
    // Time-Based Eviction Tests
    // ========================================================================

    mod time_based_eviction {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(3600, vec![7200, 5400, 1800, 600], vec![1800, 600], "1h threshold: evicts entries >1h old")]
        #[case(1800, vec![3600, 1800, 900, 300], vec![1800, 900, 300], "30m threshold: evicts entries >30m old")]
        #[case(7200, vec![10800, 7200, 3600, 1800], vec![7200, 3600, 1800], "2h threshold: evicts entries >2h old")]
        #[case(600, vec![1200, 600, 300, 60], vec![600, 300, 60], "10m threshold: evicts entries >10m old")]
        #[tokio::test]
        async fn test_time_based_eviction_thresholds(
            #[case] max_age_seconds: u64,
            #[case] entry_ages_seconds: Vec<u64>,
            #[case] expected_remaining_ages: Vec<u64>,
            #[case] description: &str,
        ) -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::TimeBased { max_age_seconds };
            let service = setup_test_service_with_strategy(strategy).await?;

            let now = irys_types::UnixTimestamp::now()?;

            // Insert entries at different ages
            let mut inserted_roots = Vec::new();
            for age in &entry_ages_seconds {
                let timestamp = now.saturating_sub_secs(*age);
                let root = insert_entry_with_timestamp(&service, timestamp)?;
                inserted_roots.push((root, *age));
            }

            let initial_count = get_cache_entry_count(&service)?;
            assert_eq!(
                initial_count,
                entry_ages_seconds.len(),
                "All entries should be inserted"
            );

            // Run eviction
            service.prune_cache_by_time(max_age_seconds)?;

            // Verify expected entries remain
            let final_count = get_cache_entry_count(&service)?;
            assert_eq!(
                final_count,
                expected_remaining_ages.len(),
                "Test case '{}': Expected {} entries, got {}",
                description,
                expected_remaining_ages.len(),
                final_count
            );

            // Verify correct entries were kept
            for age in &expected_remaining_ages {
                let exists = inserted_roots.iter().any(|(root, entry_age)| {
                    entry_age == age && cache_contains(&service, *root).unwrap_or(false)
                });
                assert!(
                    exists,
                    "Entry with age {} should remain - {}",
                    age, description
                );
            }

            Ok(())
        }

        #[tokio::test]
        async fn test_time_based_eviction_empty_cache() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::TimeBased {
                max_age_seconds: 3600,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            // Run eviction on empty cache - should not error
            service.prune_cache_by_time(3600)?;

            assert_eq!(get_cache_entry_count(&service)?, 0);
            Ok(())
        }

        #[tokio::test]
        async fn test_time_based_eviction_all_fresh() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::TimeBased {
                max_age_seconds: 3600,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            let now = irys_types::UnixTimestamp::now()?;

            // Insert 5 recent entries (all within 1 minute)
            for i in 0..5 {
                let timestamp = now.saturating_sub_secs(i * 10);
                insert_entry_with_timestamp(&service, timestamp)?;
            }

            // Evict with 1 hour threshold - should keep all
            service.prune_cache_by_time(3600)?;

            assert_eq!(get_cache_entry_count(&service)?, 5);
            Ok(())
        }

        #[tokio::test]
        async fn test_time_based_eviction_all_expired() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::TimeBased {
                max_age_seconds: 1800,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            let now = irys_types::UnixTimestamp::now()?;

            // Insert 5 old entries (all > 2 hours old)
            for i in 0..5 {
                let timestamp = now.saturating_sub_secs(7200 + i * 100);
                insert_entry_with_timestamp(&service, timestamp)?;
            }

            // Evict with 30 minute threshold - should remove all
            service.prune_cache_by_time(1800)?;

            assert_eq!(get_cache_entry_count(&service)?, 0);
            Ok(())
        }

        #[tokio::test]
        async fn test_time_based_eviction_boundary() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::TimeBased {
                max_age_seconds: 3600,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            let now = irys_types::UnixTimestamp::now()?;

            // Entry exactly at threshold (3600 seconds old)
            let at_threshold = now.saturating_sub_secs(3600);
            let root_at = insert_entry_with_timestamp(&service, at_threshold)?;

            // Entry just under threshold (3599 seconds old)
            let under_threshold = now.saturating_sub_secs(3599);
            let root_under = insert_entry_with_timestamp(&service, under_threshold)?;

            // Entry just over threshold (3601 seconds old)
            let over_threshold = now.saturating_sub_secs(3601);
            let root_over = insert_entry_with_timestamp(&service, over_threshold)?;

            service.prune_cache_by_time(3600)?;

            // Entries at or under threshold should remain
            assert!(cache_contains(&service, root_at)?);
            assert!(cache_contains(&service, root_under)?);

            // Entry over threshold should be evicted
            assert!(!cache_contains(&service, root_over)?);

            Ok(())
        }
    }

    // ========================================================================
    // Size-Based Eviction Tests
    // ========================================================================

    mod size_based_eviction {
        use super::*;
        use rstest::rstest;

        fn fill_cache_to_size(
            service: &ChunkCacheService,
            target_size: u64,
        ) -> eyre::Result<Vec<DataRoot>> {
            use rand::Rng as _;

            let chunk_size = service.config.consensus.chunk_size;
            let mut roots = Vec::new();
            let mut current_size = 0_u64;
            let mut rng = rand::thread_rng();
            let mut pending_entries = Vec::new();

            while current_size < target_size {
                let entry_size = chunk_size.min(target_size - current_size);

                // Generate unique random data_root to avoid collisions
                let random_bytes: [u8; 32] = rng.gen();
                let root = DataRoot::from(random_bytes);

                let cached = irys_database::db_cache::CachedDataRoot {
                    data_size: entry_size,
                    txid_set: vec![],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::now().unwrap(),
                };

                pending_entries.push((root, cached));
                roots.push(root);
                current_size += entry_size.div_ceil(chunk_size) * chunk_size;

                // Batch writes every 100 entries for better performance
                if pending_entries.len() >= 100 {
                    service.db.update(|wtx| {
                        for (root, cached) in pending_entries.drain(..) {
                            wtx.put::<CachedDataRoots>(root, cached)?;
                        }
                        eyre::Ok(())
                    })??;
                }
            }

            // Write any remaining entries
            if !pending_entries.is_empty() {
                service.db.update(|wtx| {
                    for (root, cached) in pending_entries {
                        wtx.put::<CachedDataRoots>(root, cached)?;
                    }
                    eyre::Ok(())
                })??;
            }

            Ok(roots)
        }

        fn get_cache_total_size(service: &ChunkCacheService) -> eyre::Result<u64> {
            let chunk_size = service.config.consensus.chunk_size;
            service.db.view_eyre(|tx| {
                let mut total_size = 0_u64;
                let mut cursor = tx.cursor_read::<CachedDataRoots>()?;
                let mut walker = cursor.walk(None)?;

                while let Some((_, cached)) = walker.next().transpose()? {
                    let chunk_count = cached.data_size.div_ceil(chunk_size);
                    total_size += chunk_count * chunk_size;
                }

                Ok(total_size)
            })
        }

        #[rstest]
        #[case(100_000, 150_000, 90_000, "150% of limit evicts to 90%")]
        #[case(100_000, 110_000, 90_000, "110% of limit evicts to 90%")]
        #[case(100_000, 200_000, 90_000, "200% of limit evicts to 90%")]
        #[case(50_000, 75_000, 45_000, "Small cache (50KB) respects 90% rule")]
        #[tokio::test]
        async fn slow_test_size_based_eviction_hysteresis(
            #[case] max_size: u64,
            #[case] initial_size: u64,
            #[case] expected_max_final_size: u64,
            #[case] description: &str,
        ) -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::SizeBased {
                max_cache_size_bytes: max_size,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            // Fill cache to initial size
            fill_cache_to_size(&service, initial_size)?;

            let initial_count = get_cache_entry_count(&service)?;
            let actual_initial_size = get_cache_total_size(&service)?;

            // Run eviction
            service.prune_cache_by_size(initial_count as u64, actual_initial_size, max_size)?;

            // Verify final size
            let final_size = get_cache_total_size(&service)?;
            let chunk_size = service.config.consensus.chunk_size;

            assert!(
                final_size <= expected_max_final_size + chunk_size,
                "Test case '{}': Expected max ~{} bytes, got {} bytes",
                description,
                expected_max_final_size,
                final_size
            );

            Ok(())
        }

        #[tokio::test]
        async fn test_size_based_eviction_under_limit() -> eyre::Result<()> {
            let max_size = 100_000;
            let strategy = irys_types::CacheEvictionStrategy::SizeBased {
                max_cache_size_bytes: max_size,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            // Fill to 80% of limit
            let initial_size = (max_size as f64 * 0.8) as u64;
            fill_cache_to_size(&service, initial_size)?;

            let count_before = get_cache_entry_count(&service)?;
            let size_before = get_cache_total_size(&service)?;

            // Run eviction - should not evict anything
            service.prune_cache_by_size(count_before as u64, size_before, max_size)?;

            let count_after = get_cache_entry_count(&service)?;
            let size_after = get_cache_total_size(&service)?;

            assert_eq!(count_before, count_after, "No entries should be evicted");
            assert_eq!(size_before, size_after, "Size should remain unchanged");

            Ok(())
        }

        #[tokio::test]
        async fn test_size_based_eviction_empty_cache() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::SizeBased {
                max_cache_size_bytes: 1_000_000,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            // Run eviction on empty cache - should not error
            service.prune_cache_by_size(0, 0, 1_000_000)?;

            assert_eq!(get_cache_entry_count(&service)?, 0);
            Ok(())
        }

        #[tokio::test]
        async fn test_size_based_eviction_fifo_order() -> eyre::Result<()> {
            let strategy = irys_types::CacheEvictionStrategy::SizeBased {
                max_cache_size_bytes: 50_000,
            };
            let service = setup_test_service_with_strategy(strategy).await?;

            let now = irys_types::UnixTimestamp::now()?;

            // Insert entries with known timestamps (oldest to newest)
            let oldest_ts = now.saturating_sub_secs(1000);
            let oldest_root = insert_entry_with_timestamp(&service, oldest_ts)?;

            let middle_ts = now.saturating_sub_secs(500);
            let middle_root = insert_entry_with_timestamp(&service, middle_ts)?;

            let newest_ts = now.saturating_sub_secs(100);
            let newest_root = insert_entry_with_timestamp(&service, newest_ts)?;

            // Fill cache over limit
            fill_cache_to_size(&service, 100_000)?;

            let count = get_cache_entry_count(&service)?;
            let size = get_cache_total_size(&service)?;

            // Evict to bring under limit
            service.prune_cache_by_size(count as u64, size, 50_000)?;

            // Oldest entry should be evicted first
            assert!(
                !cache_contains(&service, oldest_root)?,
                "Oldest entry should be evicted"
            );

            // Middle and newest might or might not be evicted depending on size,
            // but if one remains, it should be the newest
            let middle_exists = cache_contains(&service, middle_root)?;
            let newest_exists = cache_contains(&service, newest_root)?;

            if middle_exists {
                // If middle exists, newest must also exist (FIFO ordering)
                assert!(
                    newest_exists,
                    "FIFO order violated: middle exists but newest doesn't"
                );
            }

            Ok(())
        }
    }

    // ========================================================================
    // FIFO Ordering Property Tests
    // ========================================================================

    #[cfg(test)]
    mod fifo_properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(10))]
            #[test]
            fn slow_fifo_ordering_always_maintained(
                timestamps in prop::collection::vec(100_u64..1_000_000, 5..20)
            ) {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let strategy = irys_types::CacheEvictionStrategy::SizeBased {
                        max_cache_size_bytes: 10_000_000,
                    };
                    let service = setup_test_service_with_strategy(strategy)
                        .await
                        .unwrap();

                    // Insert entries with random timestamps
                    for ts in &timestamps {
                        let timestamp = irys_types::UnixTimestamp::from_secs(*ts);
                        insert_entry_with_timestamp(&service, timestamp)
                            .unwrap();
                    }

                    // Collect entries and verify sorted
                    let entries = service.collect_cache_entries_by_age().unwrap();

                    for window in entries.windows(2) {
                        prop_assert!(
                            window[0].1.cached_at <= window[1].1.cached_at,
                            "FIFO order violated: {} > {}",
                            window[0].1.cached_at.as_secs(),
                            window[1].1.cached_at.as_secs()
                        );
                    }

                    Ok::<(), proptest::test_runner::TestCaseError>(())
                })
                .unwrap();
            }
        }
    }
}
