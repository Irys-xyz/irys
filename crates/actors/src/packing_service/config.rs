use irys_types::{Config, RemotePackingConfig};
use std::time::Duration;

const DEFAULT_POLL_DURATION_MS: u64 = 1000;
const DEFAULT_UNPACKING_QUEUE_CAPACITY: usize = 1000;

#[derive(Debug, Clone)]
pub struct PackingConfig {
    pub poll_duration: Duration,
    /// Max. number of packing threads for CPU packing
    pub concurrency: u16,
    /// Max. number of chunks send to GPU packing
    #[cfg(feature = "nvidia")]
    pub max_chunks: u32,
    /// Irys chain id
    pub chain_id: u64,
    /// Configuration for remote packing hosts
    pub remotes: Vec<RemotePackingConfig>,

    /// UNPACKING PARAMS
    /// CPU thread pool size for unpacking
    pub unpacking_concurrency: u16,
    /// Unpacking queue capacity
    pub unpacking_queue_capacity: usize,
}

impl PackingConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            poll_duration: Duration::from_millis(DEFAULT_POLL_DURATION_MS),
            concurrency: config.node_config.packing.local.cpu_packing_concurrency,
            chain_id: config.consensus.chain_id,
            #[cfg(feature = "nvidia")]
            max_chunks: config.node_config.packing.local.gpu_packing_batch_size,
            remotes: config.node_config.packing.remote.clone(),
            unpacking_concurrency: config.node_config.packing.local.cpu_unpacking_concurrency,
            unpacking_queue_capacity: DEFAULT_UNPACKING_QUEUE_CAPACITY,
        }
    }
}
