use tokio::sync::mpsc;

/// Test helpers for constructing minimal packing/service wiring without
/// booting the full packing service. Useful across unit and integration tests.
pub mod test_helpers {
    use super::*;

    use crate::services::{ServiceReceivers, ServiceSenders};

    /// Build a pair of `ServiceSenders`/`ServiceReceivers` pre-wired with a packing sender channel
    /// for tests. This eliminates boilerplate across tests that need a `ServiceSenders` instance.
    pub fn build_test_service_senders() -> (ServiceSenders, ServiceReceivers) {
        let (tx_packing, _rx_packing) = mpsc::channel(1);

        let (service_senders, receivers) = ServiceSenders::new_with_packing_sender(tx_packing);

        (service_senders, receivers)
    }
}

pub use test_helpers::build_test_service_senders;
