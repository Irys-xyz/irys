use irys_types::{Config, RemotePackingConfig};
use std::time::Duration;

const DEFAULT_POLL_DURATION_MS: u64 = 1000;

#[derive(Debug, Clone)]
pub struct PackingConfig {
    pub poll_duration: Duration,
    /// Max. number of packing threads for CPU packing, max number of concurrent batches of `max_chunks` for GPU packing
    pub concurrency: u16,
    /// Max. number of chunks send to GPU packing
    #[cfg(feature = "nvidia")]
    pub max_chunks: u32,
    /// Irys chain id
    pub chain_id: u64,
    /// Configuration for remote packing hosts
    pub remotes: Vec<RemotePackingConfig>,
}

impl PackingConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            poll_duration: Duration::from_millis(DEFAULT_POLL_DURATION_MS),
            concurrency: config.node_config.packing.local.cpu_packing_concurrency,
            chain_id: config.consensus.chain_id,
            #[cfg(feature = "nvidia")]
            max_chunks: resolve_gpu_batch_size(
                config.node_config.packing.local.gpu_packing_batch_size,
                config.node_config.packing.local.cpu_packing_concurrency,
            ),
            remotes: config.node_config.packing.remote.clone(),
        }
    }
}

/// Chunks per GPU batch. `Auto` sizes batches from GPU 0 for `concurrent_batches` batches in
/// flight; when the GPU cannot be queried it falls back to `0`, which packs on the CPU.
#[cfg(feature = "nvidia")]
fn resolve_gpu_batch_size(
    size: irys_types::config::GpuPackingBatchSize,
    concurrent_batches: u16,
) -> u32 {
    use irys_types::config::GpuPackingBatchSize;
    match size {
        GpuPackingBatchSize::Fixed(chunks) => chunks,
        GpuPackingBatchSize::Auto => {
            let auto = irys_packing::CUDAConfig::from_device_default()
                .and_then(|config| config.auto_batch_size_for_device(0, concurrent_batches));
            match auto {
                Ok(chunks) => {
                    tracing::info!(
                        gpu_packing_batch_size = chunks,
                        concurrent_batches,
                        "GPU packing batch size sized from the GPU"
                    );
                    chunks
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "cannot size GPU packing batches from the GPU; packing on the CPU"
                    );
                    0
                }
            }
        }
    }
}
