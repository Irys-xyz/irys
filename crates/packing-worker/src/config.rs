use std::num::NonZeroU8;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackingWorkerConfig {
    pub bind_port: u8,
    pub bind_addr: String,

    /// shared secret
    pub secret: String,

    /// Number of CPU threads to use for data packing operations
    pub cpu_packing_concurrency: u16,

    /// Batch size for GPU-accelerated packing operations
    pub gpu_packing_batch_size: u32,

    /// Max pending requests
    pub max_pending: NonZeroU8,
}
