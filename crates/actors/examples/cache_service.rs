use std::{future::Future, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt as _};
use reth::{
    network::metered_poll_nested_stream_with_budget,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use tokio::sync::{
    mpsc::{self, UnboundedSender},
    oneshot,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, warn};

#[derive(Debug)]
pub enum CacheServiceAction {
    OnFinalizedBlock(u64, oneshot::Sender<()>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ExampleChunkCacheServiceHandle {
    pub sender: UnboundedSender<CacheServiceAction>,
}

impl ExampleChunkCacheServiceHandle {
    pub fn spawn_service(exec: TaskExecutor) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        exec.spawn_critical_with_graceful_shutdown_signal("Cache Service", |shutdown| async move {
            let cache_service = ExampleChunkCacheService {
                shutdown: shutdown,
                msg_rx: UnboundedReceiverStream::new(rx),
            };
            cache_service.await.unwrap();
            ()
        });
        ExampleChunkCacheServiceHandle { sender: tx }
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
pub struct ExampleChunkCacheService {
    pub shutdown: GracefulShutdown,
    pub msg_rx: UnboundedReceiverStream<CacheServiceAction>,
}

trait ChunkCacheService {
    fn on_handle_message(&mut self, msg: CacheServiceAction);
}

impl ChunkCacheService for ExampleChunkCacheService {
    fn on_handle_message(&mut self, msg: CacheServiceAction) {
        match msg {
            CacheServiceAction::OnFinalizedBlock(finalized_height, sender) => {
                // prune the cache below this new finalized height
                debug!("processing OnFinalizedBlock {} message!", &finalized_height);

                _ = sender
                    .send(())
                    .inspect_err(|e| warn!("RX failure for OnFinalizedBlock: {:?}", &e));
            }
            CacheServiceAction::Shutdown => todo!(),
        }
    }
}

pub const DRAIN_BUDGET: u32 = 10;

impl Future for ExampleChunkCacheService {
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

fn main() {}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    };

    use irys_types::SimpleRNG;
    use reth::tasks::TaskManager;
    use tokio::{runtime::Handle, sync::oneshot, time::sleep};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    use super::{CacheServiceAction, ExampleChunkCacheServiceHandle};
    use futures::TryFutureExt;

    async fn cache_service_test() -> eyre::Result<()> {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let task_manager = TaskManager::new(Handle::current());
        let exec = task_manager.executor();
        let handle = ExampleChunkCacheServiceHandle::spawn_service(exec);
        let (tx, rx) = oneshot::channel();
        // task_manager.graceful_shutdown();
        handle
            .sender
            .send(CacheServiceAction::OnFinalizedBlock(42, tx))?;
        let result = rx.into_future().await?;
        dbg!(result);
        Ok(())
    }

    async fn cache_service_shutdown_test() -> eyre::Result<()> {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let task_manager = TaskManager::new(Handle::current());
        let exec = task_manager.executor();

        let handle = Arc::new(ExampleChunkCacheServiceHandle::spawn_service(exec));
        let handle2 = handle.clone();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_2 = shutting_down.clone();
        Handle::current().spawn_blocking(move || {
            let mut rng = SimpleRNG::new(1337);
            let mut i = 0;
            loop {
                let (tx, _rx) = oneshot::channel();
                match handle2
                    .sender
                    .send(CacheServiceAction::OnFinalizedBlock(i, tx))
                {
                    Ok(_) => {}
                    Err(e) => {
                        if !shutting_down_2.load(Ordering::SeqCst) {
                            panic!(
                                "failed to send while not shutting down, got error {:#?}",
                                &e
                            )
                        }
                        return;
                    }
                }
                i += 1;
                thread::sleep(Duration::from_millis(rng.next_range(500).into()))
            }
        });
        sleep(Duration::from_secs(2)).await;
        shutting_down.store(true, Ordering::SeqCst);
        task_manager.graceful_shutdown();
        Ok(())
    }
}
