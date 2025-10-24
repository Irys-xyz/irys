pub mod block_discovery;
pub mod block_index_service;
pub mod block_producer;
pub mod block_tree_service;
pub mod block_validation;

pub mod cache_service;
pub mod chunk_migration_service;
pub mod commitment_refunds;
pub mod data_sync_service;
pub mod mempool_service;
pub mod mining_bus;
pub mod packing_service;
pub mod partition_mining_service;
pub mod reth_service;
pub mod services;
pub mod shadow_tx_generator;
pub mod storage_module_service;
pub mod validation_service;

pub use block_producer::*;
pub use data_sync_service::*;
pub use mempool_service::*;
pub use mining_bus::MiningBus;
pub use partition_mining_service::*;
pub use reth_ethereum_primitives;
pub use shadow_tx_generator::ShadowMetadata;
pub use storage_module_service::*;

pub use async_trait;
pub use openssl::sha;

pub mod test_helpers {
    use crate::services::{ServiceReceivers, ServiceSenders};

    /// Helper to create minimal ServiceSenders for tests that don't need actual packing/unpacking
    pub fn build_test_service_senders() -> (ServiceSenders, ServiceReceivers) {
        let (tx_packing, rx_packing) = tokio::sync::mpsc::channel(1);
        let (tx_unpacking, rx_unpacking) = tokio::sync::mpsc::channel(1);
        std::mem::forget(rx_packing);
        std::mem::forget(rx_unpacking);
        ServiceSenders::new_with_packing_sender(tx_packing, tx_unpacking)
    }
}
