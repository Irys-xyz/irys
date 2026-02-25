use std::sync::Arc;

use async_trait::async_trait;
use irys_domain::{ChunkType, StorageModule};
use irys_packing::{CUDAConfig, capacity_pack_range_cuda_c};
use irys_types::{
    Config, IrysAddress, PartitionChunkRange, partition::PartitionHash, split_interval,
};
use tokio::sync::Semaphore;
use tokio::task::yield_now;
use tracing::{debug, warn};

use crate::packing_service::config::PackingConfig;

/// CUDA/GPU-based packing strategy
pub(crate) struct CudaPackingStrategy {
    packing_config: PackingConfig,
    semaphore: Arc<Semaphore>,
    runtime_handle: tokio::runtime::Handle,
}

impl CudaPackingStrategy {
    pub(crate) fn new(
        _config: Arc<Config>,
        packing_config: PackingConfig,
        semaphore: Arc<Semaphore>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            packing_config,
            semaphore,
            runtime_handle,
        }
    }
}

#[async_trait]
impl super::PackingStrategy for CudaPackingStrategy {
    #[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module_id, chunk.range_start = *chunk_range.0.start(), chunk.range_end = *chunk_range.0.end(), partition.hash = %partition_hash))]
    async fn pack(
        &self,
        storage_module: &Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String> {
        let chunk_size = storage_module.config.consensus.chunk_size;
        assert_eq!(
            chunk_size,
            irys_types::ConsensusConfig::CHUNK_SIZE,
            "Chunk size is not aligned with C code"
        );

        let mut handles = Vec::new();

        // Split range into batches
        for chunk_range_split in split_interval(&chunk_range, self.packing_config.max_chunks)
            .unwrap()
            .iter()
        {
            let start: u32 = *(*chunk_range_split).start();
            let end: u32 = *(*chunk_range_split).end();
            let num_chunks = end - start + 1;

            debug!(
                "Packing using CUDA C implementation, start:{} end:{} (len: {})",
                &start, &end, &num_chunks
            );

            // Acquire permit for this batch
            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Failure acquiring a CUDA packing semaphore");

            let storage_module_clone = storage_module.clone();
            let chain_id = self.packing_config.chain_id;
            let entropy_iterations = storage_module.config.consensus.entropy_packing_iterations;
            let runtime_handle = self.runtime_handle.clone();

            // Pack batch on GPU
            let handle = runtime_handle.clone().spawn(async move {
                let out = runtime_handle
                    .spawn_blocking(move || -> eyre::Result<Vec<u8>> {
                        let mut out: Vec<u8> = Vec::with_capacity(
                            (num_chunks * chunk_size as u32).try_into().unwrap(),
                        );
                        capacity_pack_range_cuda_c(
                            num_chunks,
                            mining_address,
                            start as u64,
                            partition_hash,
                            entropy_iterations,
                            chain_id,
                            CUDAConfig::from_device_default()?,
                            &mut out,
                        )?;
                        eyre::Result::Ok(out)
                    })
                    .await
                    .unwrap() // TODO: error handling
                    .unwrap();

                // Write packed chunks
                for i in 0..num_chunks {
                    storage_module_clone.write_chunk(
                        (start + i).into(),
                        out[(i * chunk_size as u32) as usize
                            ..((i + 1) * chunk_size as u32) as usize]
                            .to_vec(),
                        ChunkType::Entropy,
                    );

                    // Periodic sync
                    if i % short_writes_before_sync == 0 {
                        yield_now().await;
                        crate::packing_service::sync_with_warning(
                            &storage_module_clone,
                            "CUDA packing (periodic)",
                        );
                    }
                }
                drop(permit);
            });

            handles.push(handle);

            debug!(
                target = "irys::packing::update",
                packing.start = ?start,
                packing.end = ?end,
                packing.storage_module_id = ?storage_module_id,
                packing.partition_hash = ?partition_hash,
                packing.mining_address = ?mining_address,
                packing.entropy_iterations = ?storage_module.config.consensus.entropy_packing_iterations,
                "CUDA Packed chunks"
            );
        }

        // Wait for all packing tasks to complete
        for handle in handles {
            if let Err(e) = handle.await {
                warn!(
                    target: "irys::packing",
                    "CUDA packing task failed for SM {}: {:?}", storage_module_id, e
                );
            }
        }

        Ok(())
    }
}
