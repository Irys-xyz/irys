use actix_rt::Arbiter;
use futures::future::{BoxFuture, FutureExt as _};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::task::JoinHandle as TokioJoinHandle;

enum ServiceSetState {
    Polling(Vec<ArbiterEnum>),
    ShuttingDown(BoxFuture<'static, ()>),
}

/// A set of services that can be managed together.
pub struct ServiceSet {
    state: ServiceSetState,
}

impl ServiceSet {
    pub fn new(services: Vec<ArbiterEnum>) -> Self {
        Self {
            state: ServiceSetState::Polling(services),
        }
    }

    /// Manually trigger graceful shutdown of all services.
    /// This will cause the ServiceSet to transition to shutdown state
    /// and stop all services in order when polled.
    pub fn graceful_shutdown(&mut self) -> &mut Self {
        match &mut self.state {
            ServiceSetState::Polling(services) => {
                if !services.is_empty() {
                    self.initiate_shutdown();
                }
            }
            ServiceSetState::ShuttingDown(_) => {
                // Already shutting down
            }
        };
        self
    }

    /// Wait for all services to complete without consuming the ServiceSet.
    /// Useful when you need to check state or perform operations after completion.
    pub async fn wait(&mut self) {
        futures::future::poll_fn(|cx| std::pin::Pin::new(&mut *self).poll(cx)).await
    }

    /// Check if the ServiceSet is currently shutting down
    pub fn is_shutting_down(&self) -> bool {
        matches!(self.state, ServiceSetState::ShuttingDown(_))
    }

    /// Helper to initiate shutdown sequence
    fn initiate_shutdown(&mut self) {
        if let ServiceSetState::Polling(services) = &mut self.state {
            let services_to_shutdown = std::mem::take(services);

            let shutdown_future = async move {
                for service in services_to_shutdown {
                    let name = service.name().to_string();
                    tracing::info!("Shutting down service: {}", name);
                    service.stop_and_join().await;
                    tracing::info!("Service {} shut down", name);
                }
            }
            .boxed();

            self.state = ServiceSetState::ShuttingDown(shutdown_future);
        }
    }
}

impl fmt::Debug for ServiceSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            ServiceSetState::Polling(services) => {
                let mut actix_count = 0;
                let mut tokio_count = 0;

                for service in services {
                    match service {
                        ArbiterEnum::ActixArbiter { .. } => actix_count += 1,
                        ArbiterEnum::TokioService(_) => tokio_count += 1,
                    }
                }

                write!(
                    f,
                    "ServiceSet {{ total: {}, actix: {}, tokio: {}, state: polling }}",
                    services.len(),
                    actix_count,
                    tokio_count
                )
            }
            ServiceSetState::ShuttingDown(_) => {
                write!(f, "ServiceSet {{ state: shutting_down }}")
            }
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
                if services.is_empty() {
                    // No services to manage, complete immediately
                    return std::task::Poll::Ready(());
                }

                // Check if any service has completed
                let mut completed_idx = None;
                for (idx, service) in services.iter_mut().enumerate() {
                    match service.poll_unpin(cx) {
                        std::task::Poll::Ready(_) => {
                            // A service has exited, mark for removal
                            completed_idx = Some(idx);
                            break;
                        }
                        std::task::Poll::Pending => {
                            // This service is still running
                        }
                    }
                }

                if let Some(idx) = completed_idx {
                    // Remove the completed service to avoid polling it again
                    let completed_service = services.remove(idx);
                    tracing::info!(
                        "Service {} has exited, initiating shutdown",
                        completed_service.name()
                    );

                    // Now initiate shutdown with remaining services
                    Self::initiate_shutdown(this);

                    // Re-poll immediately to start the shutdown
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
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
pub struct TokioServiceHandle {
    pub name: String,
    pub handle: TokioJoinHandle<()>,
    pub shutdown_signal: reth::tasks::shutdown::Signal,
}

#[derive(Debug)]
pub enum ArbiterEnum {
    ActixArbiter {
        arbiter: ArbiterHandle,
    },
    TokioService(TokioServiceHandle),
}

impl ArbiterEnum {
    pub fn new_tokio_service(
        name: String,
        handle: TokioJoinHandle<()>,
        shutdown_signal: reth::tasks::shutdown::Signal,
    ) -> Self {
        Self::TokioService(TokioServiceHandle {
            name,
            handle,
            shutdown_signal,
        })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::ActixArbiter { arbiter } => &arbiter.name,
            Self::TokioService(service) => &service.name,
        }
    }

    pub async fn stop_and_join(self) {
        match self {
            Self::ActixArbiter { arbiter } => {
                arbiter.stop_and_join();
            }
            Self::TokioService(service) => {
                // Fire the shutdown signal
                service.shutdown_signal.fire();

                // Wait for the task to complete
                match service.handle.await {
                    Ok(()) => {
                        tracing::debug!("Tokio service '{}' shut down successfully", service.name)
                    }
                    Err(e) => {
                        tracing::error!("Tokio service '{}' panicked: {:?}", service.name, e)
                    }
                }
            }
        }
    }

    pub fn is_tokio_service(&self) -> bool {
        matches!(self, Self::TokioService(_))
    }
}

impl Future for ArbiterEnum {
    type Output = eyre::Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            Self::ActixArbiter { .. } => {
                // Actix arbiters don't support polling for completion
                // They need to be explicitly stopped
                std::task::Poll::Pending
            }
            Self::TokioService(service) => {
                // Poll the tokio join handle
                match service.handle.poll_unpin(cx) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};

    /// Creates a service that exits when triggered via the returned sender
    fn create_controllable_service<F>(
        name: String,
        on_exit_behavior: F,
    ) -> (ArbiterEnum, oneshot::Sender<()>)
    where
        F: FnOnce() + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let (exit_tx, exit_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx => {
                    // Graceful shutdown received
                }
                _ = exit_rx => {
                    // Explicit exit triggered
                }
            }

            // Execute the behavior
            on_exit_behavior();
        });

        (
            ArbiterEnum::new_tokio_service(name, handle, shutdown_tx),
            exit_tx,
        )
    }

    /// Creates a service that runs until shutdown signal
    fn create_long_running_service<F>(name: String, on_shutdown_behavior: F) -> ArbiterEnum
    where
        F: FnOnce() + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = tokio::spawn(async move {
            // Wait for shutdown signal
            let _ = shutdown_rx.await;

            // Execute the behavior
            on_shutdown_behavior();
        });

        ArbiterEnum::new_tokio_service(name, handle, shutdown_tx)
    }

    /// Creates a service that panics when triggered
    fn create_panicking_service(name: String) -> (ArbiterEnum, oneshot::Sender<()>) {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let (panic_tx, panic_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx => {
                    // Shutdown received, exit gracefully
                }
                _ = panic_rx => {
                    panic!("Test panic!");
                }
            }
        });

        (
            ArbiterEnum::new_tokio_service(name, handle, shutdown_tx),
            panic_tx,
        )
    }

    /// Helper to poll a ServiceSet to completion
    async fn poll_service_set_to_completion(
        service_set: ServiceSet,
    ) -> tokio::task::JoinHandle<()> {
        let poll_handle = tokio::spawn(async move {
            service_set.await;
        });

        // Give services time to start
        tokio::task::yield_now().await;

        poll_handle
    }

    #[tokio::test]
    async fn test_service_exit_triggers_shutdown() {
        // Setup: Create services where one can exit on demand
        let shutdown_count = Arc::new(AtomicUsize::new(0));
        let mut services = vec![];
        // Service that exits when triggered
        let (service, exit_trigger) = create_controllable_service("early_exit".to_string(), || {});
        services.push(service);
        // Services that run until shutdown
        for i in 1..=2 {
            let count_clone = shutdown_count.clone();
            services.push(create_long_running_service(
                format!("service_{}", i),
                move || {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                },
            ));
        }
        let service_set = ServiceSet::new(services);
        let poll_handle = poll_service_set_to_completion(service_set).await;

        // Action: Trigger one service to exit
        exit_trigger.send(()).expect("Failed to trigger exit");
        tokio::time::timeout(Duration::from_secs(5), poll_handle)
            .await
            .expect("ServiceSet should complete within timeout")
            .expect("Poll task should not panic");

        // Assert: All services should be shutdown
        assert_eq!(
            shutdown_count.load(Ordering::SeqCst),
            2,
            "All long-running services should have been shutdown"
        );
    }

    #[tokio::test]
    async fn test_service_panic_triggers_shutdown() {
        // Setup: Create normal services and one that can panic
        let shutdown_count = Arc::new(AtomicUsize::new(0));
        let mut services = vec![];
        // Normal services first
        for i in 1..=2 {
            let count_clone = shutdown_count.clone();
            services.push(create_long_running_service(
                format!("normal_service_{}", i),
                move || {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                },
            ));
        }
        // Service that panics when triggered
        let (panic_service, panic_trigger) =
            create_panicking_service("panicking_service".to_string());
        services.push(panic_service);
        let service_set = ServiceSet::new(services);
        let poll_handle = poll_service_set_to_completion(service_set).await;

        // Action: Trigger a panic in one service
        let _ = panic_trigger.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), poll_handle)
            .await
            .expect("ServiceSet should complete despite panic");

        // Assert: Normal services shutdown despite panic
        assert_eq!(
            shutdown_count.load(Ordering::SeqCst),
            2,
            "Both normal services should have been shutdown"
        );
    }

    #[tokio::test]
    async fn test_manual_shutdown_preserves_order() {
        // Setup: Create services that record their shutdown order
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut services = vec![];
        // Create services that record their shutdown order
        for i in 0..3 {
            let tx_clone = tx.clone();
            let name = format!("service_{}", i);
            let name_for_closure = name.clone();

            services.push(create_long_running_service(name, move || {
                let _ = tx_clone.send(name_for_closure);
            }));
        }
        let mut service_set = ServiceSet::new(services);

        // Action: Manually trigger graceful shutdown
        service_set.graceful_shutdown();
        // Verify state changed
        assert!(
            service_set.is_shutting_down(),
            "ServiceSet should be in shutting down state"
        );

        // Poll to completion
        service_set.wait().await;
        drop(tx); // Close the channel

        // Assert: Services shutdown in correct order
        let mut shutdown_order = vec![];
        while let Some(name) = rx.recv().await {
            shutdown_order.push(name);
        }
        assert_eq!(
            shutdown_order,
            vec!["service_0", "service_1", "service_2"],
            "Services should shutdown in order"
        );
    }

    #[tokio::test]
    async fn test_empty_service_set() {
        // Setup: Create an empty ServiceSet
        let mut service_set = ServiceSet::new(vec![]);

        // Action: Poll the empty ServiceSet
        let result = service_set.wait().now_or_never();

        // Assert: Should complete immediately
        assert!(
            result.is_some(),
            "Empty ServiceSet should complete immediately"
        );
    }
}
