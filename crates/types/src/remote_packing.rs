use std::num::NonZeroU8;

use irys_primitives::Address;
use serde::{Deserialize, Serialize};

use crate::{PartitionChunkRange, H256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePackingRequest {
    pub mining_address: Address,
    pub partition_hash: H256,
    pub chunk_range: PartitionChunkRange,
    pub chain_id: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackingWorkerConfig {
    pub bind_port: u8,
    pub bind_addr: String,

    /// Number of CPU threads to use for data packing operations
    pub cpu_packing_concurrency: u16,

    /// Batch size for GPU-accelerated packing operations
    pub gpu_packing_batch_size: u32,

    /// Max pending requests
    pub max_pending: NonZeroU8,
}
