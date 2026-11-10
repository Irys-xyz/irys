use std::num::NonZeroU8;

use crate::IrysAddress;
use crate::config::GpuPackingBatchSize;
use serde::{Deserialize, Serialize};

use crate::{H256, PartitionChunkRange};

/// Chunks one remote `/pack` request may cover. The next span is a new request.
pub const REMOTE_PACK_WINDOW_CHUNKS: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePackingRequest {
    pub mining_address: IrysAddress,
    pub partition_hash: H256,
    pub chunk_range: PartitionChunkRange,
    pub chain_id: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackingWorkerConfig {
    pub bind_port: u16,
    pub bind_addr: String,

    /// Number of CPU threads to use for data packing operations
    pub cpu_packing_concurrency: u16,

    /// Chunks per GPU packing batch: a number, or `"auto"` to size batches from the GPU.
    pub gpu_packing_batch_size: GpuPackingBatchSize,

    /// Max pending requests
    pub max_pending: NonZeroU8,
}
