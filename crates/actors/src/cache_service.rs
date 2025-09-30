use irys_database::{
    db::IrysDatabaseExt as _,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{CachedChunks, CachedDataRoots, IngressProofs},
};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::{
    Config, DataLedger, DatabaseProvider, LedgerChunkOffset, TokioServiceHandle, GIGABYTE,
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

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for chunk cache service");
                    break;
                }
                // Handle messages
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

    fn on_epoch_processed(&self, epoch_snapshot: Arc<EpochSnapshot>) -> eyre::Result<()> {
        // Find the block height of the first block to add data to the active submit ledger slots
        let ledger_id = DataLedger::Submit;
        let first_unexpired_slot_index: u64 = epoch_snapshot
            .get_first_unexpired_slot_index(ledger_id)
            .try_into()
            .unwrap();
        let chunk_offset =
            first_unexpired_slot_index * self.config.consensus.num_chunks_in_partition;

        // Check to see if the first overlapping block in our first active submit ledger slot is in the block index
        let mut prune_height: Option<u64> = None;
        if let Some(latest) = self.block_index_guard.read().get_latest_item() {
            let submit_ledger_max_chunk_offset = latest.ledgers[ledger_id].total_chunks;
            if submit_ledger_max_chunk_offset > chunk_offset {
                // If the chunk_offset is in the block index look up the block_bounds
                let block_bounds = self
                    .block_index_guard
                    .read()
                    .get_block_bounds(ledger_id, LedgerChunkOffset::from(chunk_offset))
                    .expect("Should be able to get block bounds as max_chunk_offset was checked");
                prune_height = Some((block_bounds.height - 1).try_into().unwrap());
            }
        }

        // If it wasn't in the index, we'll have to check the canonical
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
            let (block_height, _ledger_max_offset) = found_block.unwrap();
            prune_height = Some(block_height - 1);
        }

        // Prune the data root cache at the start of the submit ledger
        self.prune_data_root_cache(prune_height.unwrap())
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
        // TODO: add code to prune chunks BEFORE their absolute expiry epoch based on the number of chunks
        // (and add a config value to the cache config for this ^)
        Ok(())
    }

    fn prune_data_root_cache(&self, prune_height: u64) -> eyre::Result<()> {
        let mut chunks_pruned: u64 = 0;
        let write_tx = self.db.tx_mut()?;
        // Iterate all CachedDataRoots and compute the latest block height they were included in
        let mut cursor = write_tx.cursor_write::<CachedDataRoots>()?;
        let mut walker = cursor.walk(None)?;
        while let Some((data_root, cached)) = walker.next().transpose()? {
            // Determine pruning horizon: prefer last inclusion height from block_set,
            // otherwise fall back to expiry_height (if set). If neither is available, skip pruning.
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
            if horizon.is_none() {
                debug!(
                    ?data_root,
                    "Skipping prune for data root without inclusion or expiry"
                );
                continue;
            }
            let max_height: u64 = horizon.unwrap();

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
                // Remove ingress proofs
                write_tx.delete::<IngressProofs>(data_root, None)?;
                // Remove cached chunks and index
                chunks_pruned = chunks_pruned
                    .saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
                // Remove cached data root entry
                write_tx.delete::<CachedDataRoots>(data_root, None)?;
            }
        }
        debug!(?chunks_pruned, "Pruned chunks");
        write_tx.commit()?;

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
        app_state::DatabaseProvider, Base64, Config, DataTransactionHeader, IrysBlockHeader,
        NodeConfig, TxChunkOffset, UnpackedChunk,
    };
    use std::sync::{Arc, RwLock};

    // This test prevents a regression of bug: mempool-only data roots (with empty block_set field)
    // are pruned once prune_height > 0 and they should not be pruned!
    #[tokio::test]
    async fn does_not_prune_unconfirmed_data_roots() -> eyre::Result<()> {
        // Minimal config and database
        let node_config = NodeConfig::testing();
        let config = Config::new(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader {
            data_size: 64,
            ..Default::default()
        };
        db.update(|wtx| {
            database::cache_data_root(wtx, &tx_header, None)?;
            eyre::Ok(())
        })??;

        // Also cache one chunk + index so pruning is observable
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

        // Sanity check: entries exist before pruning
        db.view(|rtx| -> eyre::Result<()> {
            let has_root = rtx.get::<CachedDataRoots>(tx_header.data_root)?.is_some();
            eyre::ensure!(has_root, "CachedDataRoots missing before prune");
            Ok(())
        })??;

        // Build minimal guards (not used by prune_data_root_cache)
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

        // Construct service (we won't drive the async loop; just call the internal prune)
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

        // Ensure root still exists
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
        // Minimal config and database
        let node_config = NodeConfig::testing();
        let config = Config::new(node_config);
        let db_env = open_or_create_db(
            irys_testing_utils::utils::temporary_directory(None, false),
            IrysTables::ALL,
            None,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Create a data root cached via mempool path (no block header -> empty block_set)
        let tx_header = DataTransactionHeader {
            data_size: 64,
            ..Default::default()
        };
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

        // Build minimal guards (not used by prune_data_root_cache)
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
}
