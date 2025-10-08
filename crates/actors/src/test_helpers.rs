use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;

/// Test helpers for constructing minimal packing/service wiring without
/// booting the full packing service. Useful across unit and integration tests.
pub mod test_helpers {
    use super::*;
    use crate::packing::{Internals, PackingConfig, PackingHandle};
    use crate::services::{ServiceReceivers, ServiceSenders};

    /// Build a minimal `PackingHandle` for tests (bounded channel, default internals).
    /// This does NOT spawn any packing workers; it's meant purely to satisfy dependencies
    /// where tests need a handle present (e.g., wiring `ServiceSenders`).
    pub fn build_test_packing_handle(config: &irys_types::Config) -> PackingHandle {
        let (tx_packing, _rx_packing) = mpsc::channel(1);

        let internals = Internals {
            pending_jobs: Arc::new(RwLock::new(HashMap::new())),
            semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            active_workers: Arc::new(AtomicUsize::new(0)),
            // Construct a PackingConfig using a cloned Arc of the provided config
            config: PackingConfig::new(&Arc::new(config.clone())),
        };

        PackingHandle::from_parts(tx_packing, internals)
    }

    /// Build a pair of `ServiceSenders`/`ServiceReceivers` pre-wired with a minimal `PackingHandle`
    /// for tests. This eliminates boilerplate across tests that need a `ServiceSenders` instance.
    pub fn build_test_service_senders(
        _config: &irys_types::Config,
    ) -> (ServiceSenders, ServiceReceivers) {
        let (tx_packing, _rx_packing) = mpsc::channel(1);

        let (service_senders, receivers) = ServiceSenders::new_with_packing_sender(tx_packing);
        // Provide the handle so code that needs wait_for_packing continues to work

        (service_senders, receivers)
    }
}

pub use test_helpers::{build_test_packing_handle, build_test_service_senders};
