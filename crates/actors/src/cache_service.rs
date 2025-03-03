use std::{future::Future, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt as _};

use irys_database::{
    db_cache::DataRootLRUEntry,
    delete_cached_chunks_by_data_root, get_chunk_cache_size, get_pd_cache_size,
    tables::{DataRootLRU, IngressProofs, ProgrammableDataCache, ProgrammableDataLRU},
};
use irys_types::{Config, DatabaseProvider, GIGABYTE};

use reth::{
    network::metered_poll_nested_stream_with_budget,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use reth_db::cursor::DbCursorRO;
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
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
            cache_service.await.unwrap();
            ()
        });
        ChunkCacheServiceHandle { sender: tx }
    }

    #[inline]
    pub async fn send(
        &self,
        msg: CacheServiceAction,
    ) -> std::result::Result<(), tokio::sync::mpsc::error::SendError<CacheServiceAction>> {
        self.sender.send(msg)
    }
}

#[derive(Debug)]
pub struct ChunkCacheService {
    pub shutdown: GracefulShutdown,
    pub msg_rx: UnboundedReceiverStream<CacheServiceAction>,
    pub db: DatabaseProvider,
    pub config: Config,
}

trait ChunkCache {
    fn on_handle_message(&mut self, msg: CacheServiceAction);
}

impl ChunkCache for ChunkCacheService {
    fn on_handle_message(&mut self, msg: CacheServiceAction) {
        match msg {
            CacheServiceAction::OnFinalizedBlock(finalized_height, sender) => {
                let res = self.prune_cache(finalized_height);
                match sender {
                    Some(sender) => {
                        let _ = sender
                            .send(res)
                            .inspect_err(|e| warn!("RX failure for OnFinalizedBlock: {:?}", &e));
                    }
                    None => {}
                }
            }
        }
    }
}

// TODO: improve this, store state in-memory (derived from DB state) to reduce db lookups
// take into account other usage (i.e API/gossip) for cache expiry
// partition expiries to prevent holding the write lock for too long
impl ChunkCacheService {
    fn prune_cache(&self, finalized_height: u64) -> eyre::Result<()> {
        let prune_height = finalized_height.saturating_sub(self.config.cache_clean_lag as u64);
        self.prune_data_root_cache(prune_height)?;
        self.prune_pd_cache(prune_height)?;
        let ((ccc, ccs), (pcc, pcs), ips) = self.db.view_eyre(|tx| {
            Ok((
                get_chunk_cache_size(tx, self.config.chunk_size)?,
                get_pd_cache_size(tx, self.config.chunk_size)?,
                tx.entries::<IngressProofs>()?,
            ))
        })?;
        info!(
            ?finalized_height,
            "Chunk cache: {} chunks ({:.3} GB), PD: {} chunks ({:.3} GB) {} ingress proofs",
            &ccc,
            (&ccs / GIGABYTE as u64),
            &pcc,
            (&pcs / GIGABYTE as u64),
            &ips
        );
        Ok(())
    }

    fn prune_data_root_cache(&self, prune_height: u64) -> eyre::Result<()> {
        let mut chunks_pruned = 0;
        let write_tx = self.db.tx_mut()?;
        let mut cursor = write_tx.cursor_write::<DataRootLRU>()?;
        let mut walker = cursor.walk(None)?;
        while let Some((data_root, DataRootLRUEntry { last_height, .. })) =
            walker.next().transpose()?
        {
            if last_height < prune_height {
                debug!(
                    "expiring ingress proof {} (last used height: {}, prune_height: {})",
                    &data_root, &last_height, prune_height
                );
                write_tx.delete::<DataRootLRU>(data_root, None)?;
                write_tx.delete::<IngressProofs>(data_root, None)?;
                // delete the cached chunks
                chunks_pruned += delete_cached_chunks_by_data_root(&write_tx, data_root)?;
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
        while let Some((k, expiry_height)) = walker.next().transpose()? {
            if expiry_height < prune_height {
                debug!(
                    "expiring PD chunk {:?} (expiry: {}, prune height: {})",
                    &k, &expiry_height, prune_height
                );
                write_tx.delete::<ProgrammableDataLRU>(k, None)?;
                write_tx.delete::<ProgrammableDataCache>(k, None)?;
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
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();

        match this.shutdown.poll_unpin(cx) {
            Poll::Ready(guard) => {
                debug!("Shutting down!");
                // do shutdown stuff
                // process all remaining tasks
                loop {
                    match this.msg_rx.poll_next_unpin(cx) {
                        Poll::Ready(Some(msg)) => this.on_handle_message(msg),
                        Poll::Ready(None) => break,
                        Poll::Pending => break,
                    }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use irys_database::tables::{CachedChunks, CachedChunksIndex, IrysTables};
    use irys_database::walk_all;
    use irys_database::{cache_chunk, open_or_create_db};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{Base64, DatabaseProvider, TxChunkOffset, UnpackedChunk, H256};
    use reth_db::cursor::DbDupCursorRO;
    use reth_db::transaction::DbTx;
    use reth_db::transaction::DbTxMut;
    use reth_db::Database as _;

    #[tokio::test]
    async fn test_cache_prune() -> eyre::Result<()> {
        let dir = setup_tracing_and_temp_dir(Some("test_cache_prune"), false);
        let db = open_or_create_db(dir.path(), IrysTables::ALL, None).unwrap();
        let provider = DatabaseProvider(Arc::new(db));
        let data_root = H256::repeat_byte(1);
        let chunks: Vec<UnpackedChunk> = (0..6)
            .into_iter()
            .map(|i| UnpackedChunk {
                data_root: match i {
                    5 => H256::repeat_byte(2),
                    _ => data_root,
                },
                data_size: 100,
                data_path: Base64(vec![0u8, i]),
                bytes: Base64(vec![1u8, 2u8]),
                tx_offset: TxChunkOffset(i as u32),
            })
            .collect::<Vec<_>>();
        let write_tx = provider.tx_mut()?;
        for chunk in chunks.iter() {
            cache_chunk(&write_tx, chunk)?;
        }
        write_tx.commit()?;

        let read_tx = provider.tx()?;
        let mut cursor = read_tx.cursor_dup_read::<CachedChunksIndex>()?;
        let mut walker = cursor.walk_dup(Some(data_root), None)?; // iterate a specific key's subkeys
        while let Some((k, c)) = walker.next().transpose()? {
            dbg!(&k, &c);
            provider.update_eyre(|tx| {
                tx.delete::<CachedChunks>(c.meta.chunk_path_hash, None)?;
                Ok(tx.delete::<CachedChunksIndex>(k, Some(c))?) // delete just a specific dupsort entry by subkey
            })?;
        }

        let read_tx = provider.tx()?;
        // should only have chunk number 5 (data root repeat_byte(2))
        let cc = walk_all::<CachedChunks, _>(&read_tx);
        let cci = walk_all::<CachedChunksIndex, _>(&read_tx);
        dbg!(&cc, &cci);

        Ok(())
    }

    //     use std::{
    //         sync::{
    //             atomic::{AtomicBool, Ordering},
    //             Arc,
    //         },
    //         thread,
    //         time::Duration,
    //     };

    //     use irys_types::SimpleRNG;
    //     use reth::tasks::TaskManager;
    //     use tokio::{runtime::Handle, sync::oneshot, time::sleep};
    //     use tracing::level_filters::LevelFilter;
    //     use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    //     use super::{CacheServiceAction, ChunkCacheServiceHandle};
    //     use futures::TryFutureExt;

    //     #[tokio::test(flavor = "multi_thread")]
    //     async fn cache_service_test() -> eyre::Result<()> {
    //         let _ = SubscriberBuilder::default()
    //             .with_max_level(LevelFilter::DEBUG)
    //             .finish()
    //             .try_init();

    //         let task_manager = TaskManager::new(Handle::current());
    //         let exec = task_manager.executor();
    //         let handle = ChunkCacheServiceHandle::spawn_service(exec, db);
    //         let (tx, rx) = oneshot::channel();
    //         // task_manager.graceful_shutdown();
    //         handle
    //             .sender
    //             .send(CacheServiceAction::OnFinalizedBlock(42, tx))?;
    //         let result = rx.into_future().await?;
    //         dbg!(result);
    //         Ok(())
    //     }

    //     #[tokio::test(flavor = "multi_thread")]
    //     async fn cache_service_shutdown_test() -> eyre::Result<()> {
    //         let _ = SubscriberBuilder::default()
    //             .with_max_level(LevelFilter::DEBUG)
    //             .finish()
    //             .try_init();

    //         let task_manager = TaskManager::new(Handle::current());
    //         let exec = task_manager.executor();

    //         let handle = Arc::new(ChunkCacheServiceHandle::spawn_service(exec));
    //         let handle2 = handle.clone();
    //         let shutting_down = Arc::new(AtomicBool::new(false));
    //         let shutting_down_2 = shutting_down.clone();
    //         Handle::current().spawn_blocking(move || {
    //             let mut rng = SimpleRNG::new(1337);
    //             let mut i = 0;
    //             loop {
    //                 let (tx, _rx) = oneshot::channel();
    //                 match handle2
    //                     .sender
    //                     .send(CacheServiceAction::OnFinalizedBlock(i, tx))
    //                 {
    //                     Ok(_) => {}
    //                     Err(e) => {
    //                         if !shutting_down_2.load(Ordering::SeqCst) {
    //                             panic!(
    //                                 "failed to send while not shutting down, got error {:#?}",
    //                                 &e
    //                             )
    //                         }
    //                         return;
    //                     }
    //                 }
    //                 i += 1;
    //                 thread::sleep(Duration::from_millis(rng.next_range(500).into()))
    //             }
    //         });
    //         sleep(Duration::from_secs(2)).await;
    //         shutting_down.store(true, Ordering::SeqCst);
    //         task_manager.graceful_shutdown();
    //         Ok(())
    //     }
}
