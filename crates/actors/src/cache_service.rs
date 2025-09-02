use irys_database::{
    db::IrysDatabaseExt as _,
    db_cache::DataRootLRUEntry,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{
        CachedChunks, DataRootLRU, IngressProofs, ProgrammableDataCache, ProgrammableDataLRU,
    },
};
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot};
use irys_types::{Config, DataLedger, DatabaseProvider, TokioServiceHandle, GIGABYTE};
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
            let submit_ledger_max_chunk_offset = latest.ledgers[ledger_id].max_chunk_offset;
            if submit_ledger_max_chunk_offset > chunk_offset {
                // If the chunk_offset is in the block index look up the block_bounds
                let block_bounds = self
                    .block_index_guard
                    .read()
                    .get_block_bounds(ledger_id, chunk_offset)
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
                let ledger_max_offset = block.data_ledgers[ledger_id].max_chunk_offset;
                if ledger_max_offset <= chunk_offset {
                    Some((block_entry.height, ledger_max_offset))
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
        debug!("Pruning cache for height {}", &migration_height);
        let prune_height = migration_height
            .saturating_sub(u64::from(self.config.node_config.cache.cache_clean_lag));
        self.prune_pd_cache(prune_height)?;
        let (
            (chunk_cache_count, chunk_cache_size),
            (pd_cache_count, pd_cache_size),
            ingress_proof_count,
        ) = self.db.view_eyre(|tx| {
            Ok((
                get_cache_size::<CachedChunks, _>(tx, self.config.consensus.chunk_size)?,
                get_cache_size::<ProgrammableDataCache, _>(tx, self.config.consensus.chunk_size)?,
                tx.entries::<IngressProofs>()?,
            ))
        })?;
        info!(
            ?migration_height,
            "Chunk cache: {} chunks ({:.3} GB), PD: {} chunks ({:.3} GB) {} ingress proofs",
            chunk_cache_count,
            (chunk_cache_size / GIGABYTE as u64),
            pd_cache_count,
            (pd_cache_size / GIGABYTE as u64),
            ingress_proof_count
        );
        Ok(())
    }

    fn prune_data_root_cache(&self, prune_height: u64) -> eyre::Result<()> {
        let mut chunks_pruned: u64 = 0;
        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<DataRootLRU>()?;
        let mut walker = cursor.walk(None)?;
        while let Some((data_root, DataRootLRUEntry { last_height, .. })) =
            walker.next().transpose()?
        {
            debug!(
                "Processing data root {} last height: {}, prune height: {}",
                &data_root, &last_height, &prune_height
            );
            if last_height < prune_height {
                debug!(
                    ?data_root,
                    ?last_height,
                    ?prune_height,
                    "expiring ingress proof",
                );
                write_tx.delete::<DataRootLRU>(data_root, None)?;
                write_tx.delete::<IngressProofs>(data_root, None)?;
                // delete the cached chunks
                chunks_pruned = chunks_pruned
                    .saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
            }
        }
        debug!(?chunks_pruned, "Pruned chunks");
        write_tx.commit()?;

        Ok(())
    }

    fn prune_pd_cache(&self, prune_height: u64) -> eyre::Result<()> {
        debug!("processing OnBlockMigrated PD {} message!", &prune_height);

        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<ProgrammableDataLRU>()?;
        let mut walker = cursor.walk(None)?;
        while let Some((global_offset, expiry_height)) = walker.next().transpose()? {
            if expiry_height < prune_height {
                debug!(
                    ?prune_height,
                    ?expiry_height,
                    ?global_offset,
                    "expiring PD chunk",
                );
                write_tx.delete::<ProgrammableDataLRU>(global_offset, None)?;
                write_tx.delete::<ProgrammableDataCache>(global_offset, None)?;
            }
        }
        write_tx.commit()?;

        Ok(())
    }
}
