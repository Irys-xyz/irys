mod block_pool;
mod integration;
pub(crate) mod util;

/// Test helper to create minimal ServiceSenders for p2p tests
pub(crate) mod test_helpers {
    use irys_actors::services::{ServiceReceivers, ServiceSenders};

    pub(crate) fn build_test_service_senders() -> (ServiceSenders, ServiceReceivers) {
        let (tx_packing, rx_packing) = tokio::sync::mpsc::channel(1);
        let (tx_unpacking, rx_unpacking) = tokio::sync::mpsc::channel(1);
        std::mem::forget(rx_packing);
        std::mem::forget(rx_unpacking);
        ServiceSenders::new_with_packing_sender(tx_packing, tx_unpacking)
    }
}
