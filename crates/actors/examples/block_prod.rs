use std::{future::Future, pin::Pin, task::Poll, time::Duration};

use futures::{FutureExt, StreamExt as _};
use irys_types::{SimpleRNG, H256};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use tokio::{
    sync::{
        mpsc::{self, UnboundedSender},
        oneshot,
    },
    time::sleep,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, warn};

#[derive(Debug)]
pub enum BlockProducerAction {
    OnSolutionFound(H256, oneshot::Sender<()>),
}

#[derive(Debug, Clone)]
pub struct BlockProducerHandle {
    pub sender: UnboundedSender<BlockProducerAction>,
}

impl BlockProducerHandle {
    pub fn spawn_service(exec: TaskExecutor) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        exec.spawn_critical_with_graceful_shutdown_signal("Cache Service", |shutdown| async move {
            let cache_service = ExampleBlockProducer {
                fut: None,
                shutdown: shutdown,
                msg_rx: UnboundedReceiverStream::new(rx),
            };
            cache_service.await.unwrap();
            ()
        });
        BlockProducerHandle { sender: tx }
    }

    #[inline]
    pub async fn send(
        &self,
        msg: BlockProducerAction,
    ) -> std::result::Result<(), tokio::sync::mpsc::error::SendError<BlockProducerAction>> {
        self.sender.send(msg)
    }
}

use std::fmt::Debug;

pub struct ExampleBlockProducer {
    pub shutdown: GracefulShutdown,
    pub fut: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    pub msg_rx: UnboundedReceiverStream<BlockProducerAction>,
}

impl Debug for ExampleBlockProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExampleBlockProducer")
            .field("shutdown", &self.shutdown)
            .field(
                "fut",
                match self.fut {
                    Some(_) => &"Some(future)",
                    None => &"None",
                },
            )
            .field("msg_rx", &self.msg_rx)
            .finish()
    }
}

trait BlockProducer {
    // returns a future that must be polled before another message is processed
    fn on_handle_message(
        &self,
        msg: BlockProducerAction,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl BlockProducer for ExampleBlockProducer {
    fn on_handle_message(
        &self,
        msg: BlockProducerAction,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        match msg {
            BlockProducerAction::OnSolutionFound(solution_hash, sender) => {
                return Box::pin((async move || {
                    debug!("processing OnSolutionFound {} message!", &solution_hash);
                    sleep(Duration::from_millis(
                        SimpleRNG::new(1000).next_range(1_000).into(),
                    ))
                    .await;
                    _ = sender
                        .send(())
                        .inspect_err(|e| warn!("RX failure for OnSolutionFound: {:?}", &e));
                })())
            }
        }
    }
}

impl Future for ExampleBlockProducer {
    type Output = eyre::Result<String>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();

        // if the block production future is done, or no future is set, continue
        match this.fut.as_mut() {
            Some(fut) => match fut.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    debug!("Block producer future finished");
                    this.fut = None; // make sure we set it to `None` so we can't poll it again
                }
                Poll::Pending => return Poll::Pending,
            },
            None => {}
        };

        // shutdown
        match this.shutdown.poll_unpin(cx) {
            Poll::Ready(guard) => {
                debug!("Shutting down!");
                drop(guard);
                return Poll::Ready(Ok("Graceful shutdown".to_owned()));
            }
            Poll::Pending => {}
        };

        match this.msg_rx.poll_next_unpin(cx) {
            // we only evaluate one message at a time
            Poll::Ready(Some(msg)) => {
                let fut = this.on_handle_message(msg);
                this.fut = Some(fut);

                // make sure we're woken up again (so it starts polling the block prod future, and in case there are more pending messages)
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(None) => Poll::Pending, // no more pending values
            Poll::Pending => Poll::Pending,
        }
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

    use irys_types::{SimpleRNG, H256};
    use reth::tasks::TaskManager;
    use tokio::{runtime::Handle, sync::oneshot, time::sleep};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    use futures::TryFutureExt;

    use crate::{BlockProducerAction, BlockProducerHandle};

    async fn block_prod_actor_test() -> eyre::Result<()> {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let task_manager = TaskManager::new(Handle::current());
        let exec = task_manager.executor();
        let handle = BlockProducerHandle::spawn_service(exec);
        let (tx, rx) = oneshot::channel();
        // task_manager.graceful_shutdown();
        handle
            .sender
            .send(BlockProducerAction::OnSolutionFound(H256::zero(), tx))?;
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

        let handle = Arc::new(BlockProducerHandle::spawn_service(exec));
        let handle2 = handle.clone();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_2 = shutting_down.clone();
        Handle::current().spawn_blocking(move || {
            let mut rng = SimpleRNG::new(1337);
            let mut i = 0;
            loop {
                let (tx, _rx) = oneshot::channel();
                match handle2.sender.send(BlockProducerAction::OnSolutionFound(
                    H256::repeat_byte(i),
                    tx,
                )) {
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
