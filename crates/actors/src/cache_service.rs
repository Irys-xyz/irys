use core::{future::Future, task::Poll, time::Duration};
use eyre::Context;
use futures::{FutureExt as _, StreamExt as _};
use irys_database::{
    db_cache::DataRootLRUEntry,
    delete_cached_chunks_by_data_root, get_cache_size,
    tables::{
        CachedChunks, DataRootLRU, IngressProofs, ProgrammableDataCache, ProgrammableDataLRU,
    },
};
use irys_types::{Config, DatabaseProvider, GIGABYTE};
use reth::{
    network::metered_poll_nested_stream_with_budget,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::*;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub enum CacheServiceAction {
    OnFinalizedBlock(u64, Option<oneshot::Sender<eyre::Result<()>>>),
}

pub type CacheServiceSender = UnboundedSender<CacheServiceAction>;

#[derive(Debug, Clone)]
pub struct ChunkCacheServiceHandle {
    pub sender: CacheServiceSender,
}

impl ChunkCacheServiceHandle {
    pub fn spawn_service(
        tx: CacheServiceSender,
        rx: UnboundedReceiver<CacheServiceAction>,
        exec: TaskExecutor,
        db: DatabaseProvider,
        config: Config,
    ) -> Self {
        exec.spawn_critical_with_graceful_shutdown_signal("Cache Service", |shutdown| async move {
            let cache_service = ChunkCacheService {
                shutdown,
                db,
                config,
                msg_rx: UnboundedReceiverStream::new(rx),
            };
            cache_service
                .await
                .wrap_err("Error running cache_service")
                .unwrap();
        });
        Self { sender: tx }
    }

    pub fn send(
        &self,
        msg: CacheServiceAction,
    ) -> core::result::Result<(), tokio::sync::mpsc::error::SendError<CacheServiceAction>> {
        self.sender.send(msg)
    }
}

#[derive(Debug)]
pub struct ChunkCacheService {
    pub config: Config,
    pub db: DatabaseProvider,
    pub msg_rx: UnboundedReceiverStream<CacheServiceAction>,
    pub shutdown: GracefulShutdown,
}

// TODO: improve this, store state in-memory (derived from DB state) to reduce db lookups
// take into account other usage (i.e API/gossip) for cache expiry
// partition expiries to prevent holding the write lock for too long
// use a two-stage pass - read only find, and then a write-locked prune (with checks to make sure expiry hasn't been updated inbetween)
impl ChunkCacheService {
    fn on_handle_message(&self, msg: CacheServiceAction) {
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
        let prune_height = finalized_height.saturating_sub(u64::from(self.config.cache_clean_lag));
        self.prune_data_root_cache(prune_height)?;
        self.prune_pd_cache(prune_height)?;
        let (
            (chunk_cache_count, chunk_cache_size),
            (pd_cache_count, pd_cache_size),
            ingress_proof_count,
        ) = self.db.view_eyre(|tx| {
            Ok((
                get_cache_size::<CachedChunks, _>(tx, self.config.chunk_size)?,
                get_cache_size::<ProgrammableDataCache, _>(tx, self.config.chunk_size)?,
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
                    "expiring ingress proof ",
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
}

pub const DRAIN_BUDGET: u32 = 10;

impl Future for ChunkCacheService {
    type Output = eyre::Result<String>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        let this = self.get_mut();

        match this.shutdown.poll_unpin(cx) {
            Poll::Ready(guard) => {
                debug!("Shutting down CacheService");
                // process all remaining tasks
                while let Poll::Ready(Some(msg)) = this.msg_rx.poll_next_unpin(cx) {
                    this.on_handle_message(msg);
                }
                drop(guard);
                return Poll::Ready(Ok("Graceful shutdown".to_owned()));
            }
            Poll::Pending => {}
        }

        let mut time_taken = Duration::ZERO;

        // process `DRAIN_BUDGET` messages before yielding
        let maybe_more_handle_messages = metered_poll_nested_stream_with_budget!(
            time_taken,
            "cache",
            "cache service channel",
            DRAIN_BUDGET,
            this.msg_rx.poll_next_unpin(cx),
            |msg| this.on_handle_message(msg),
            error!("cache channel closed");
        );

        debug!("took {:?} to process cache service messages", &time_taken);

        if maybe_more_handle_messages {
            // make sure we're woken up again
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Pending
    }
}
