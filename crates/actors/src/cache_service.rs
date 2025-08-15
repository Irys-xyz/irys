use irys_database::{
    db::IrysDatabaseExt as _,
    db_cache::DataRootLRUEntry,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{
        CachedChunks, CachedDataRoots, DataRootLRU, IngressProofs, ProgrammableDataCache,
        ProgrammableDataLRU,
    },
};
use irys_types::{Config, DatabaseProvider, TokioServiceHandle, GIGABYTE};
use reth::tasks::shutdown::Shutdown;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub enum CacheServiceAction {
    OnFinalizedBlock(u64, Option<oneshot::Sender<eyre::Result<()>>>),
}

pub type CacheServiceSender = UnboundedSender<CacheServiceAction>;

#[derive(Debug)]
pub struct ChunkCacheService {
    pub config: Config,
    pub db: DatabaseProvider,
    pub msg_rx: UnboundedReceiver<CacheServiceAction>,
    pub shutdown: Shutdown,
}

impl ChunkCacheService {
    pub fn spawn_service(
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
                db,
                config,
                msg_rx: rx,
            };
            cache_service
                .start()
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
            CacheServiceAction::OnFinalizedBlock(finalized_height, sender) => {
                let res = self.prune_cache(finalized_height);
                let Some(sender) = sender else { return };
                if let Err(error) = sender.send(res) {
                    warn!(?error, "RX failure for OnFinalizedBlock");
                }
            }
        }
    }

    fn prune_cache(&self, finalized_height: u64) -> eyre::Result<()> {
        let prune_height = finalized_height
            .saturating_sub(u64::from(self.config.node_config.cache.cache_clean_lag));
        self.prune_data_root_cache(prune_height)?;
        self.prune_pd_cache(prune_height)?;
        self.prune_orphaned_cached_data_roots()?;
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
            ?finalized_height,
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
        debug!("processing OnFinalizedBlock PD {} message!", &prune_height);

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

    fn prune_orphaned_cached_data_roots(&self) -> eyre::Result<()> {
        // TTL for orphaned cached roots without ingress proof
        const ORPHAN_TTL_MILLIS: u128 = 10 * 60 * 1000; // 10 minutes
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| eyre::eyre!("System time before UNIX_EPOCH"))?
            .as_millis();

        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<CachedDataRoots>()?;
        let mut walker = cursor.walk(None)?;
        let mut pruned = 0u64;

        while let Some((data_root, cached)) = walker.next().transpose()? {
            // Skip if there is an LRU entry (it will be pruned elsewhere)
            if write_tx.get::<DataRootLRU>(data_root)?.is_some() {
                continue;
            }
            // Skip if there is any ingress proof present
            if write_tx.get::<IngressProofs>(data_root)?.is_some() {
                continue;
            }
            // Check TTL
            if now_millis.saturating_sub(cached.timestamp) > ORPHAN_TTL_MILLIS {
                // delete cached chunks and index
                pruned =
                    pruned.saturating_add(delete_cached_chunks_by_data_root(&write_tx, data_root)?);
                // delete the CachedDataRoots entry
                write_tx.delete::<CachedDataRoots>(data_root, None)?;
            }
        }

        if pruned > 0 {
            debug!(
                pruned_chunks = pruned,
                "Pruned orphaned cached data roots and chunks"
            );
        }
        write_tx.commit()?;
        Ok(())
    }
}
