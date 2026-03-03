pub(super) mod cpu;
pub(super) mod remote;

#[cfg(feature = "nvidia")]
pub(super) mod cuda;

use std::sync::Arc;

use async_trait::async_trait;
use irys_domain::StorageModule;
use irys_types::{IrysAddress, PartitionChunkRange, partition::PartitionHash};

/// Trait for different packing strategies
#[async_trait]
pub(super) trait PackingStrategy: Send + Sync {
    /// Pack chunks using this strategy
    async fn pack(
        &self,
        storage_module: &Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String>;
}
