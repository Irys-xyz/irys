use irys_actors::packing_service::services::unpacking::InternalUnpackingService;
use irys_actors::services::{ServiceReceivers, ServiceSenders};
use irys_types::Config;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Build service senders with a specific config for tests that need custom consensus parameters
///
/// Spawns a real InternalUnpackingService to handle unpacking requests.
pub(crate) fn build_test_service_senders_with_config(
    config: Arc<Config>,
) -> (ServiceSenders, ServiceReceivers) {
    let (tx_packing, rx_packing) = mpsc::channel(1);
    let (tx_unpacking, rx_unpacking) = mpsc::channel(1);

    // Keep packing receiver alive but don't process (tests don't use packing)
    std::mem::forget(rx_packing);

    // Spawn real unpacking service for tests
    let mut unpacking_service = InternalUnpackingService::new(config);

    // Attach the receiver loop to process unpacking requests
    let runtime_handle = tokio::runtime::Handle::current();
    let _unpacking_handle = unpacking_service.attach_receiver_loop(
        runtime_handle.clone(),
        rx_unpacking,
        tx_unpacking.clone(),
    );

    // Spawn the actual unpacking controller workers to process jobs
    let _controller_handles = unpacking_service.spawn_unpacking_controllers(runtime_handle);

    // Leak the handles to keep the service running for the test duration
    std::mem::forget(_unpacking_handle);
    std::mem::forget(_controller_handles);

    let (service_senders, receivers) =
        ServiceSenders::new_with_packing_sender(tx_packing, tx_unpacking);

    (service_senders, receivers)
}
