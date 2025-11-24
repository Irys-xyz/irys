#[cfg(feature = "nvidia")]
pub use irys_c::nvidia_gpu_config::NvidiaGpuConfig;

#[derive(Debug, Clone)]
pub struct CUDAConfig {
    pub blocks: i32,
    pub threads_per_block: i32,
}

impl CUDAConfig {
    pub fn from_device(device_id: i32) -> eyre::Result<Self> {
        let config = NvidiaGpuConfig::query_device(device_id).map_err(|e| eyre::eyre!("{}", e))?;
        // note: testing has shown the best performance is achieved when we mirror our workload to the physical architecture - so we determine the threads to use based off the hardware CUDA cores per SM.
        Ok(Self {
            blocks: config.multiprocessor_count,
            threads_per_block: config.cores_per_sm(),
        })
    }
    pub fn from_device_default() -> eyre::Result<Self> {
        Self::from_device(0)
    }
}
