use std::sync::Arc;

use async_trait::async_trait;
use irys_domain::{ChunkType, StorageModule};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_types::IrysAddress;
use irys_types::{Config, PartitionChunkOffset, PartitionChunkRange, partition::PartitionHash};
use tokio::sync::{Notify, Semaphore};
use tokio::task::yield_now;
use tracing::{debug, trace, warn};

use super::common::PackingParams;

/// CPU-based packing strategy
pub(crate) struct CpuPackingStrategy {
    params: PackingParams,
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    runtime_handle: tokio::runtime::Handle,
}

impl CpuPackingStrategy {
    pub(crate) fn new(
        config: Arc<Config>,
        semaphore: Arc<Semaphore>,
        notify: Arc<Notify>,
        runtime_handle: tokio::runtime::Handle,
        _chain_id: u64,
    ) -> Self {
        Self {
            params: PackingParams::from_config(&config),
            semaphore,
            notify,
            runtime_handle,
        }
    }
}

#[async_trait]
impl super::PackingStrategy for CpuPackingStrategy {
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
        let range_start = *chunk_range.0.start();
        let range_end = *chunk_range.0.end();

        let mut handles = Vec::new();

        for i in range_start..=range_end {
            // Periodic sync to avoid memory buildup
            if i % short_writes_before_sync == 0 {
                crate::packing_service::sync_with_warning(storage_module, "CPU packing (periodic)");
                yield_now().await;
            }

            // Acquire permit for this chunk
            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "Semaphore acquisition failed (channel closed)".to_string())?;

            let chunk_size = self.params.chunk_size;
            let entropy_iterations = self.params.entropy_iterations;
            let chain_id = self.params.chain_id;
            let storage_module_clone = storage_module.clone();
            let notify = self.notify.clone();

            // Pack in blocking thread pool
            let handle = self.runtime_handle
                .clone()
                .spawn_blocking(move || {
                    let mut out = Vec::with_capacity(chunk_size);
                    compute_entropy_chunk(
                        mining_address,
                        i as u64,
                        partition_hash.0,
                        entropy_iterations,
                        chunk_size,
                        &mut out,
                        chain_id,
                    );

                    debug!(
                        target: "irys::packing::progress",
                        "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {} iterations {}",
                        &i,
                        &storage_module_id,
                        &partition_hash,
                        &mining_address,
                        entropy_iterations
                    );

                    storage_module_clone.write_chunk(
                        PartitionChunkOffset::from(i),
                        out,
                        ChunkType::Entropy,
                    );
                    drop(permit);
                    notify.notify_waiters();
                });

            handles.push(handle);

            // Log progress periodically
            crate::packing_service::log_packing_progress(
                "CPU",
                i,
                &chunk_range,
                storage_module_id,
                &partition_hash,
                &mining_address,
            );
        }

        // Wait for all packing tasks to complete
        for handle in handles {
            if let Err(e) = handle.await {
                warn!(
                    target: "irys::packing",
                    "CPU packing task failed for SM {}: {:?}", storage_module_id, e
                );
            }
        }

        trace!(
            target: "irys::packing::done",
            "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {:?} iterations {}",
            chunk_range.0.start(),
            chunk_range.0.end(),
            &storage_module_id,
            &partition_hash,
            &mining_address,
            self.params.entropy_iterations
        );

        Ok(())
    }
}
