#[cfg(feature = "nvidia")]
pub use irys_c::nvidia_gpu_config::NvidiaGpuConfig;

use irys_types::ConsensusConfig;

/// CUDA packing parameters for one device.
///
/// Each GPU thread packs one whole chunk (a serial hash chain), so throughput comes from having many
/// chunks in flight: the packer launches one thread per chunk of the batch, and the batch size
/// (`auto_batch_size`) sets how many chunks are in flight.
#[derive(Debug, Clone)]
pub struct CUDAConfig {
    /// Streaming multiprocessors on the device.
    pub sm_count: i32,
    /// CUDA cores per SM.
    pub cores_per_sm: i32,
    /// Threads per block of the packing kernel.
    pub threads_per_block: i32,
}

/// 256 KiB.
const CHUNK_BYTES: u64 = ConsensusConfig::CHUNK_SIZE;

impl CUDAConfig {
    /// Threads per block of the packing kernel. On an RTX 5090 at 1M iterations, 128 is within ~4%
    /// of the best shape at every batch size that fills whole waves.
    pub const THREADS_PER_BLOCK: i32 = 128;

    /// Chunks in flight per CUDA core for the automatic batch size. One per core (the old launch
    /// shape) leaves each SM scheduler a single warp, too few to hide memory latency: on an RTX 5090,
    /// 4 per core packs ~20% more chunks/s than 1 per core.
    pub const CHUNKS_PER_CORE: u64 = 4;

    /// Fraction of the device's free memory that the automatic batch size may use.
    const VRAM_FRACTION: f64 = 0.8;

    /// Device memory a packer needs beyond its chunks: two output staging slabs.
    #[cfg(feature = "nvidia")]
    const SLAB_BYTES: u64 = 2 * crate::cuda_packer::SLAB_CHUNKS as u64 * CHUNK_BYTES;
    #[cfg(not(feature = "nvidia"))]
    const SLAB_BYTES: u64 = 2 * 512 * CHUNK_BYTES;

    #[cfg(feature = "nvidia")]
    pub fn from_device(device_id: i32) -> eyre::Result<Self> {
        let config = NvidiaGpuConfig::query_device(device_id).map_err(|e| eyre::eyre!("{}", e))?;
        Ok(Self {
            sm_count: config.multiprocessor_count,
            cores_per_sm: config.cores_per_sm(),
            threads_per_block: Self::THREADS_PER_BLOCK,
        })
    }

    #[cfg(feature = "nvidia")]
    pub fn from_device_default() -> eyre::Result<Self> {
        Self::from_device(0)
    }

    /// Chunks per full wave of the launch shape: one block of `threads_per_block` per SM.
    pub fn wave(&self) -> u64 {
        self.sm_count.max(1) as u64 * self.threads_per_block.max(1) as u64
    }

    /// The automatic GPU packing batch size for this device, with `concurrent_batches` batches in
    /// flight at once (each holds its own device buffers; output streams to the host in 128 MiB
    /// slabs, so host memory does not grow with the batch).
    ///
    /// `min(CHUNKS_PER_CORE x cores, VRAM_FRACTION x free device memory / concurrent_batches)`,
    /// rounded down to whole waves. A partial wave runs as long as a full one: on an RTX 5090,
    /// 32 768 chunks pack slower than 21 760.
    pub fn auto_batch_size(&self, free_device_bytes: u64, concurrent_batches: u16) -> u32 {
        let batches = u64::from(concurrent_batches.max(1));
        let target =
            self.sm_count.max(1) as u64 * self.cores_per_sm.max(1) as u64 * Self::CHUNKS_PER_CORE;
        let vram_per_batch = (free_device_bytes as f64 * Self::VRAM_FRACTION) as u64 / batches;
        let vram_cap = vram_per_batch.saturating_sub(Self::SLAB_BYTES) / CHUNK_BYTES;
        let chunks = target.min(vram_cap);
        let wave = self.wave();
        let tpb = self.threads_per_block.max(1) as u64;
        let rounded = if chunks >= wave {
            chunks / wave * wave
        } else {
            (chunks / tpb * tpb).max(tpb.min(chunks))
        };
        rounded.clamp(1, u64::from(u32::MAX)) as u32
    }

    /// [`Self::auto_batch_size`] for `device_id`, querying its free memory now.
    #[cfg(feature = "nvidia")]
    pub fn auto_batch_size_for_device(
        &self,
        device_id: i32,
        concurrent_batches: u16,
    ) -> eyre::Result<u32> {
        let (free, _total) = irys_c::nvidia_gpu_config::get_device_memory(device_id)
            .map_err(|e| eyre::eyre!("{}", e))?;
        Ok(self.auto_batch_size(free as u64, concurrent_batches))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtx5090() -> CUDAConfig {
        CUDAConfig {
            sm_count: 170,
            cores_per_sm: 128,
            threads_per_block: CUDAConfig::THREADS_PER_BLOCK,
        }
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn auto_batch_is_four_chunks_per_core_when_memory_allows() {
        // 170 SMs x 128 cores x 4 = 87 040 = 4 waves (21.3 GiB)
        assert_eq!(rtx5090().auto_batch_size(30 * GIB, 1), 87_040);
    }

    #[test]
    fn auto_batch_shares_device_memory_between_concurrent_batches() {
        // 30 GiB * 0.8 / 2 - 256 MiB = 11.75 GiB = 48 128 chunks -> 2 waves
        assert_eq!(rtx5090().auto_batch_size(30 * GIB, 2), 43_520);
        // / 4: 5.75 GiB = 23 552 chunks -> 1 wave
        assert_eq!(rtx5090().auto_batch_size(30 * GIB, 4), 21_760);
    }

    #[test]
    fn auto_batch_below_one_wave_rounds_to_whole_blocks() {
        // 4 GiB * 0.8 - 256 MiB = 3 020.8 MiB = 12 083 chunks -> 12 032 (94 blocks of 128)
        assert_eq!(rtx5090().auto_batch_size(4 * GIB, 1), 12_032);
    }

    #[test]
    fn auto_batch_never_exceeds_the_core_target() {
        let c = CUDAConfig {
            sm_count: 4,
            cores_per_sm: 64,
            threads_per_block: CUDAConfig::THREADS_PER_BLOCK,
        };
        // 4 SMs x 64 cores x 4 = 1 024 chunks; a wave is 512
        assert_eq!(c.auto_batch_size(64 * GIB, 1), 1_024);
    }

    #[test]
    fn auto_batch_is_at_least_one_chunk() {
        assert_eq!(rtx5090().auto_batch_size(0, 4), 1);
    }
}
