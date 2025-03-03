use std::{future::Future, pin::Pin, sync::Arc, task::Poll, time::Duration};

use base58::ToBase58;
use futures::{lock::Mutex, FutureExt, StreamExt as _};
use irys_actors::mempool_service::TxIngressError;
use irys_types::IrysTransactionHeader;
use reth::{
    network::metered_poll_nested_stream_with_budget,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};

/// Represents a future outputting unit type and is sendable.
type ServiceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Represents a stream of Service futures.
type ServiceStream = ReceiverStream<ServiceFuture>;

/// A service that performs Service jobs.
///
/// This listens for incoming Service jobs and executes them.
///
/// This should be spawned as a task: [`ServiceTask::run`]
#[derive(Clone)]
pub struct ServiceTask {
    service_jobs: Arc<Mutex<ServiceStream>>,
}

impl std::fmt::Debug for ServiceTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceTask")
            .field("service_jobs", &"...")
            .finish()
    }
}

impl ServiceTask {
    /// Creates a new cloneable task pair
    pub fn new() -> (ServiceJobSender, Self) {
        let (tx, rx) = mpsc::channel(1);
        (ServiceJobSender { tx }, Self::with_receiver(rx))
    }

    /// Creates a new task with the given receiver.
    pub fn with_receiver(jobs: mpsc::Receiver<Pin<Box<dyn Future<Output = ()> + Send>>>) -> Self {
        Self {
            service_jobs: Arc::new(Mutex::new(ReceiverStream::new(jobs))),
        }
    }

    /// Executes all new Service jobs that come in.
    ///
    /// This will run as long as the channel is alive and is expected to be spawned as a task.
    pub async fn run(self) {
        while let Some(task) = self.service_jobs.lock().await.next().await {
            task.await;
        }
    }
}

/// A sender new type for sending Service jobs to [`ServiceTask`].
#[derive(Debug)]
pub struct ServiceJobSender {
    tx: mpsc::Sender<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl ServiceJobSender {
    /// Sends the given job to the Service task.
    pub async fn send(&self, job: Pin<Box<dyn Future<Output = ()> + Send>>) -> eyre::Result<()> {
        self.tx
            .send(job)
            .await
            .map_err(|_| eyre::eyre!("service task unavailable"))
        // .map_err(|_| TransactionValidatorError::ServiceServiceUnreachable)
    }
    pub fn try_send(&self, job: Pin<Box<dyn Future<Output = ()> + Send>>) -> eyre::Result<()> {
        self.tx
            .try_send(job)
            .map_err(|_| eyre::eyre!("service task unavailable"))
    }
}

#[derive(Debug)]
pub enum MempoolServiceAction {
    OnTxIngress(
        IrysTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct MempoolServiceHandle {
    pub sender: Sender<MempoolServiceAction>,
}

impl MempoolServiceHandle {
    pub fn spawn_service(exec: TaskExecutor) -> Self {
        let (tx, rx) = mpsc::channel(200);

        let worker_threads = 3;
        let (task_tx, task) = ServiceTask::new();

        // Spawn validation tasks
        // TODO: these should probably be blocking
        for i in 0..worker_threads {
            let task = task.clone();
            // whyyy
            let name = Box::leak(format!("example mempool worker thread {}", i).into_boxed_str());
            exec.spawn_critical_with_graceful_shutdown_signal(name, |shutdown| async move {
                // TODO: figure out proper graceful shutdown here
                let f = task.run();
                let graceful_guard = tokio::select! {
                    _ = f => None,
                    guard = shutdown => Some(guard)
                };
                // perform shutdown operations here
                drop(graceful_guard)
            });
        }

        exec.spawn_critical_with_graceful_shutdown_signal(
            "Example mempool Service",
            |shutdown| async move {
                let cache_service = ExampleMempoolService {
                    shutdown,
                    msg_rx: ReceiverStream::new(rx),
                    task_tx,
                };
                cache_service.await.unwrap();
                ()
            },
        );
        MempoolServiceHandle { sender: tx }
    }

    #[inline]
    pub async fn send(
        &self,
        msg: MempoolServiceAction,
    ) -> std::result::Result<(), tokio::sync::mpsc::error::TrySendError<MempoolServiceAction>> {
        self.sender.try_send(msg)
    }
}

#[derive(Debug)]
pub struct ExampleMempoolService {
    pub shutdown: GracefulShutdown,
    pub msg_rx: ReceiverStream<MempoolServiceAction>,
    pub task_tx: ServiceJobSender,
}

trait MempoolService {
    fn on_handle_message(&mut self, msg: MempoolServiceAction);
}

impl MempoolService for ExampleMempoolService {
    fn on_handle_message(&mut self, msg: MempoolServiceAction) {
        match msg {
            MempoolServiceAction::OnTxIngress(tx, sender) => {
                let _ = self
                    .task_tx
                    .try_send(Box::pin(async move {
                        let res = {
                            debug!("tx id {}", &tx.id.0.to_base58());
                            if !tx.is_signature_valid() {
                                Err(TxIngressError::InvalidSignature)
                            } else {
                                Ok(())
                            }
                        };
                        _ = sender.send(res)
                    }))
                    .inspect_err(|e| warn!("processing failure for OnTxIngress: {:?}", &e));
            }
            MempoolServiceAction::Shutdown => todo!(),
        }
    }
}

pub const DRAIN_BUDGET: u32 = 10;

impl Future for ExampleMempoolService {
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
            "mempool",
            "mempool service channel",
            DRAIN_BUDGET,
            this.msg_rx.poll_next_unpin(cx),
            |msg| this.on_handle_message(msg),
            error!("mempool channel closed");
        );

        debug!("took {:?} to process mempool service messages", &time_taken);

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

    use irys_actors::mempool_service::TxIngressError;
    use irys_types::{irys::IrysSigner, Config, IrysTransaction, IrysTransactionHeader, SimpleRNG};
    use reth::tasks::TaskManager;
    use tokio::{runtime::Handle, sync::oneshot, time::sleep};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    use futures::TryFutureExt;

    use crate::{ExampleMempoolService, MempoolServiceAction, MempoolServiceHandle};

    #[tokio::test(flavor = "multi_thread")]
    async fn mempool_service_test() -> eyre::Result<()> {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let task_manager = TaskManager::new(Handle::current());
        let exec = task_manager.executor();
        let handle = MempoolServiceHandle::spawn_service(exec);
        let (tx, rx) = oneshot::channel();
        // task_manager.graceful_shutdown();
        handle
            .sender
            .send(MempoolServiceAction::OnTxIngress(
                IrysTransactionHeader::default(),
                tx,
            ))
            .await?;
        let result = rx.into_future().await?;
        assert_eq!(result, Err(TxIngressError::InvalidSignature));

        let tx_h = IrysTransactionHeader::default();
        let tx = IrysTransaction {
            header: tx_h,
            ..Default::default()
        };
        let i_tx = IrysSigner::random_signer(&Config::testnet()).sign_transaction(tx)?;
        let (tx, rx) = oneshot::channel();
        handle
            .sender
            .send(MempoolServiceAction::OnTxIngress(i_tx.header, tx))
            .await?;
        let result = rx.into_future().await?;
        assert_eq!(result, Ok(()));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mempool_service_shutdown_test() -> eyre::Result<()> {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let task_manager = TaskManager::new(Handle::current());
        let exec = task_manager.executor();

        let handle = Arc::new(MempoolServiceHandle::spawn_service(exec));
        let handle2 = handle.clone();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let shutting_down_2 = shutting_down.clone();
        Handle::current().spawn_blocking(move || {
            let mut rng = SimpleRNG::new(1337);
            let mut i = 0;
            loop {
                let (tx, _rx) = oneshot::channel();
                match handle2.sender.try_send(MempoolServiceAction::OnTxIngress(
                    IrysTransactionHeader::default(),
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
