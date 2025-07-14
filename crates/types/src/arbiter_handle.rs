use actix_rt::Arbiter;
use futures::future::{BoxFuture, FutureExt};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::task::JoinHandle as TokioJoinHandle;

enum ServiceSetState {
    Polling(Vec<ArbiterEnum>),
    ShuttingDown(BoxFuture<'static, ()>),
}

pub struct ServiceSet {
    state: ServiceSetState,
}

impl ServiceSet {
    pub fn new(services: Vec<ArbiterEnum>) -> Self {
        Self {
            state: ServiceSetState::Polling(services),
        }
    }
}

impl Future for ServiceSet {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // Get mutable access to self
        let this = self.get_mut();
        
        match &mut this.state {
            ServiceSetState::Polling(services) => {
                // Check if any service has completed
                for service in services.iter_mut() {
                    let service_pin = std::pin::Pin::new(service);
                    match service_pin.poll(cx) {
                        std::task::Poll::Ready(_) => {
                            // A service has exited, start shutdown sequence
                            let services_to_shutdown = std::mem::take(services);
                            
                            // Create shutdown future that processes services sequentially
                            let shutdown_future = async move {
                                for service in services_to_shutdown {
                                    let name = service.name().to_string();
                                    tracing::info!("Shutting down service: {}", name);
                                    service.stop_and_join().await;
                                    tracing::info!("Service {} shut down", name);
                                }
                            }.boxed();
                            
                            // Transition to shutting down state
                            this.state = ServiceSetState::ShuttingDown(shutdown_future);
                            
                            // Re-poll immediately to start the shutdown
                            cx.waker().wake_by_ref();
                            return std::task::Poll::Pending;
                        }
                        std::task::Poll::Pending => {
                            // This service is still running
                        }
                    }
                }
                
                // All services are still running
                std::task::Poll::Pending
            }
            ServiceSetState::ShuttingDown(shutdown_future) => {
                // Poll the shutdown future
                match shutdown_future.poll_unpin(cx) {
                    std::task::Poll::Ready(()) => std::task::Poll::Ready(()),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum ArbiterEnum {
    ActixArbiter {
        arbiter: ArbiterHandle,
    },
    TokioService {
        name: String,
        handle: TokioJoinHandle<()>,
        shutdown_signal: reth::tasks::shutdown::Signal,
    },
}

impl ArbiterEnum {
    pub fn new_tokio_service(
        name: String,
        handle: TokioJoinHandle<()>,
        shutdown_signal: reth::tasks::shutdown::Signal,
    ) -> Self {
        ArbiterEnum::TokioService {
            name,
            handle,
            shutdown_signal,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            ArbiterEnum::ActixArbiter { arbiter } => &arbiter.name,
            ArbiterEnum::TokioService { name, .. } => name,
        }
    }

    pub async fn stop_and_join(self) {
        match self {
            ArbiterEnum::ActixArbiter { arbiter } => {
                arbiter.stop_and_join();
            }
            ArbiterEnum::TokioService {
                name,
                handle,
                shutdown_signal,
            } => {
                // Fire the shutdown signal
                shutdown_signal.fire();

                // Wait for the task to complete
                match handle.await {
                    Ok(()) => {
                        tracing::debug!("Tokio service '{}' shut down successfully", name)
                    }
                    Err(e) => {
                        tracing::error!("Tokio service '{}' panicked: {:?}", name, e)
                    }
                }
            }
        }
    }

    pub fn is_tokio_service(&self) -> bool {
        matches!(self, ArbiterEnum::TokioService { .. })
    }

    /// Checks if the tokio service has finished
    pub fn is_tokio_service_finished(&self) -> bool {
        match self {
            ArbiterEnum::TokioService { handle, .. } => handle.is_finished(),
            _ => false,
        }
    }
}

impl Future for ArbiterEnum {
    type Output = eyre::Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // Use unsafe to access the inner fields
        // This is safe because we're not moving the data, just accessing it
        let this = unsafe { self.get_unchecked_mut() };

        match this {
            ArbiterEnum::ActixArbiter { .. } => {
                // Actix arbiters don't support polling for completion
                // They need to be explicitly stopped
                std::task::Poll::Pending
            }
            ArbiterEnum::TokioService { handle, .. } => {
                // Poll the tokio join handle
                let handle_pin = std::pin::Pin::new(handle);
                match handle_pin.poll(cx) {
                    std::task::Poll::Ready(res) => {
                        std::task::Poll::Ready(res.map_err(|err| eyre::Report::new(err)))
                    }
                    std::task::Poll::Pending => std::task::Poll::Pending,
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ArbiterHandle {
    inner: Arc<Mutex<Option<Arbiter>>>,
    pub name: String,
}

impl Clone for ArbiterHandle {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ArbiterHandle {
    pub fn new(value: Arbiter, name: String) -> Self {
        Self {
            name,
            inner: Arc::new(Mutex::new(Some(value))),
        }
    }

    pub fn take(&self) -> Arbiter {
        let mut guard = self.inner.lock().unwrap();
        if let Some(value) = guard.take() {
            value
        } else {
            panic!("Value already consumed");
        }
    }

    pub fn stop_and_join(self) {
        let arbiter = self.take();
        arbiter.stop();
        arbiter.join().unwrap();
    }
}

#[derive(Debug)]
pub struct CloneableJoinHandle<T> {
    inner: Arc<Mutex<Option<JoinHandle<T>>>>,
}

impl<T> Clone for CloneableJoinHandle<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> CloneableJoinHandle<T> {
    pub fn new(handle: JoinHandle<T>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(handle))),
        }
    }

    pub fn join(&self) -> thread::Result<T> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(handle) = guard.take() {
            handle.join()
        } else {
            Err(Box::new("Thread handle already consumed!"))
        }
    }
}

impl<T> From<JoinHandle<T>> for CloneableJoinHandle<T> {
    fn from(handle: JoinHandle<T>) -> Self {
        Self::new(handle)
    }
}
