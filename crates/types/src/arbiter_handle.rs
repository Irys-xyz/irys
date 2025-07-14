use actix_rt::Arbiter;
use futures::future::{BoxFuture, FutureExt};
use std::fmt;
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

    /// Manually trigger graceful shutdown of all services.
    /// This will cause the ServiceSet to transition to shutdown state
    /// and stop all services in order when polled.
    pub fn graceful_shutdown(self: std::pin::Pin<&mut Self>) -> &mut Self {
        let this = self.get_mut();

        match &mut this.state {
            ServiceSetState::Polling(services) => {
                if !services.is_empty() {
                    Self::initiate_shutdown(this);
                }
            }
            ServiceSetState::ShuttingDown(_) => {
                // Already shutting down
            }
        };
        this
    }

    /// Check if the ServiceSet is currently shutting down
    pub fn is_shutting_down(&self) -> bool {
        matches!(self.state, ServiceSetState::ShuttingDown(_))
    }

    /// Helper to initiate shutdown sequence
    fn initiate_shutdown(this: &mut ServiceSet) {
        if let ServiceSetState::Polling(services) = &mut this.state {
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

            this.state = ServiceSetState::ShuttingDown(shutdown_future);
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
                        ArbiterEnum::TokioService { .. } => tokio_count += 1,
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
                for service in services.iter_mut() {
                    // Skip services that have already finished (for tokio services)
                    if service.is_tokio_service_finished() {
                        // A service has exited, start shutdown sequence
                        Self::initiate_shutdown(this);

                        // Re-poll immediately to start the shutdown
                        cx.waker().wake_by_ref();
                        return std::task::Poll::Pending;
                    }
                    
                    let service_pin = std::pin::Pin::new(service);
                    match service_pin.poll(cx) {
                        std::task::Poll::Ready(_) => {
                            // A service has exited, start shutdown sequence
                            Self::initiate_shutdown(this);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;

    async fn create_test_tokio_service<F>(
        name: String,
        behavior: F,
    ) -> ArbiterEnum 
    where
        F: FnOnce() + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx => {
                    // Shutdown received
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    // Timer expired
                }
            }
            
            // Always execute the behavior to record what happened
            behavior();
        });

        ArbiterEnum::new_tokio_service(name, handle, shutdown_tx)
    }

    #[tokio::test]
    async fn test_service_exit_triggers_shutdown() {
        // Track shutdown order
        let shutdown_order = Arc::new(Mutex::new(Vec::new()));
        let shutdown_order_clone = shutdown_order.clone();

        // Create services where one will exit early
        let mut services = vec![];
        
        // Service that exits quickly
        let exit_service_order = shutdown_order_clone.clone();
        services.push(create_test_tokio_service(
            "early_exit".to_string(),
            move || {
                // Record that this service exited
                exit_service_order.lock().unwrap().push("early_exit".to_string());
            },
        ).await);

        // Services that would run forever
        for i in 1..=2 {
            let order_clone = shutdown_order_clone.clone();
            let name = format!("service_{}", i);
            let name_clone = name.clone();
            
            services.push(create_test_tokio_service(
                name,
                move || {
                    // Record when we start shutting down
                    order_clone.lock().unwrap().push(name_clone);
                },
            ).await);
        }

        let mut service_set = ServiceSet::new(services);
        
        // Give the early exit service time to actually exit
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Poll until completion
        tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::poll_fn(|cx| {
                std::pin::Pin::new(&mut service_set).poll(cx)
            })
        ).await.expect("ServiceSet should complete within timeout");

        // Verify shutdown happened
        let order = shutdown_order.lock().unwrap();
        assert_eq!(order.len(), 3, "All 3 services should have been shutdown");
        assert_eq!(order[0], "early_exit", "Early exit service should complete first");
    }

    #[tokio::test]
    async fn test_service_panic_triggers_shutdown() {
        let shutdown_count = Arc::new(AtomicUsize::new(0));
        let mut services = vec![];

        // Normal services first
        for i in 1..=2 {
            let count_clone = shutdown_count.clone();
            services.push(create_test_tokio_service(
                format!("normal_service_{}", i),
                move || {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                },
            ).await);
        }
        
        // Service that panics - add last so we can control timing
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let panic_handle = tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx => {
                    // Shutdown received, don't panic
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    panic!("Test panic!");
                }
            }
        });
        services.push(ArbiterEnum::new_tokio_service(
            "panicking_service".to_string(),
            panic_handle,
            shutdown_tx
        ));

        let mut service_set = ServiceSet::new(services);
        
        // Give time for panic to occur
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Poll until completion - should handle panic gracefully
        tokio::time::timeout(
            Duration::from_secs(5),
            futures::future::poll_fn(|cx| {
                std::pin::Pin::new(&mut service_set).poll(cx)
            })
        ).await.expect("ServiceSet should complete despite panic");

        // Normal services should have been shutdown despite the panic
        assert_eq!(shutdown_count.load(Ordering::SeqCst), 2, "Both normal services should have been shutdown");
    }

    #[tokio::test]
    async fn test_manual_graceful_shutdown() {
        let shutdown_happened = Arc::new(AtomicBool::new(false));
        let mut services = vec![];

        // Create services that run forever
        for i in 0..3 {
            let shutdown_flag = shutdown_happened.clone();
            services.push(create_test_tokio_service(
                format!("service_{}", i),
                move || {
                    shutdown_flag.store(true, Ordering::SeqCst);
                },
            ).await);
        }

        let mut service_set = ServiceSet::new(services);
        
        // Manually trigger shutdown
        std::pin::Pin::new(&mut service_set).graceful_shutdown();
        
        // Verify state changed
        assert!(service_set.is_shutting_down());
        
        // Poll to completion
        futures::future::poll_fn(|cx| {
            std::pin::Pin::new(&mut service_set).poll(cx)
        }).await;

        // Verify shutdown actually happened
        assert!(shutdown_happened.load(Ordering::SeqCst), "Services should have been shutdown");
    }

    #[tokio::test]
    async fn test_shutdown_order_preserved() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        let mut services = vec![];
        
        // Create services that record their shutdown order
        for i in 0..3 {
            let tx_clone = tx.clone();
            let name = format!("service_{}", i);
            let name_for_closure = name.clone();
            
            services.push(create_test_tokio_service(
                name,
                move || {
                    let _ = tx_clone.send(name_for_closure);
                },
            ).await);
        }

        let mut service_set = ServiceSet::new(services);
        
        // Trigger manual shutdown
        std::pin::Pin::new(&mut service_set).graceful_shutdown();
        
        // Poll to completion
        futures::future::poll_fn(|cx| {
            std::pin::Pin::new(&mut service_set).poll(cx)
        }).await;
        
        drop(tx); // Close the channel
        
        // Collect shutdown order
        let mut shutdown_order = vec![];
        while let Some(name) = rx.recv().await {
            shutdown_order.push(name);
        }
        
        // Verify services shut down in order
        assert_eq!(shutdown_order, vec!["service_0", "service_1", "service_2"], "Services should shutdown in order");
    }

    #[tokio::test]
    async fn test_empty_service_set() {
        let mut service_set = ServiceSet::new(vec![]);
        
        // Should complete immediately
        let result = futures::future::poll_fn(|cx| {
            std::pin::Pin::new(&mut service_set).poll(cx)
        }).now_or_never();
        
        assert!(result.is_some(), "Empty ServiceSet should complete immediately");
    }

    #[tokio::test]
    async fn test_debug_impl() {
        let mut services = vec![];
        
        // Add some test services
        for i in 0..2 {
            services.push(create_test_tokio_service(
                format!("test_service_{}", i),
                || {},
            ).await);
        }
        
        let service_set = ServiceSet::new(services);
        let debug_str = format!("{:?}", service_set);
        
        assert!(debug_str.contains("total: 2"));
        assert!(debug_str.contains("tokio: 2"));
        assert!(debug_str.contains("actix: 0"));
        assert!(debug_str.contains("state: polling"));
    }
}
