use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use irys_domain::{EntropyRejection, EntropyWrite, StorageModule};
use irys_packing::CudaPackerPool;
use irys_types::{
    Config, IrysAddress, PartitionChunkOffset, PartitionChunkRange, partition::PartitionHash,
    split_interval,
};
use tokio::sync::Semaphore;
use tokio::task::yield_now;
use tracing::{debug, warn};

use crate::packing_service::config::PackingConfig;

/// Chunks buffered between the GPU and the entropy writer (256 MiB). When the disk falls behind,
/// the GPU side waits instead of buffering more.
const CHUNKS_IN_FLIGHT: usize = 1024;

/// CUDA/GPU-based packing strategy
pub(crate) struct CudaPackingStrategy {
    packing_config: PackingConfig,
    semaphore: Arc<Semaphore>,
    runtime_handle: tokio::runtime::Handle,
    /// Idle packers, reused across batches.
    packers: Arc<CudaPackerPool>,
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
            packers: Default::default(),
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
        // A refused write fails the whole range. Batches already running
        // finish, but the loop does not start another one.
        let stop = Arc::new(AtomicBool::new(false));

        // A zero step is an error. Returning it keeps the controller alive;
        // the caller requeues the range instead of unwinding this task.
        let batches =
            split_interval(&chunk_range, self.packing_config.max_chunks).map_err(|err| {
                format!("CUDA packing batch split failed for SM {storage_module_id}: {err}")
            })?;
        for chunk_range_split in batches.iter() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
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
            if stop.load(Ordering::Relaxed) {
                drop(permit);
                break;
            }

            let storage_module_clone = storage_module.clone();
            let stop_flag = Arc::clone(&stop);
            let chain_id = self.packing_config.chain_id;
            let entropy_iterations = storage_module.config.consensus.entropy_packing_iterations;
            let runtime_handle = self.runtime_handle.clone();
            let packers = Arc::clone(&self.packers);
            let packer_capacity = self.packing_config.max_chunks;
            let chunk_bytes =
                usize::try_from(chunk_size).expect("protocol-fixed chunk_size fits usize");

            // Pack on the GPU and write each chunk as its slab arrives: the batch is never held
            // in host memory as a whole, and the GPU fills the next slab while chunks are written.
            let handle = runtime_handle.clone().spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CHUNKS_IN_FLIGHT);
                let gpu = runtime_handle.spawn_blocking(move || -> eyre::Result<()> {
                    packers.with_packer(packer_capacity, |packer| {
                        packer.pack(
                            num_chunks,
                            mining_address,
                            u64::from(start),
                            partition_hash,
                            entropy_iterations,
                            chain_id,
                            |_first, slab| {
                                for chunk in slab.chunks_exact(chunk_bytes) {
                                    // fails once the writer stopped: stop packing this batch
                                    tx.blocking_send(chunk.to_vec())
                                        .map_err(|_| eyre::eyre!("entropy writer stopped"))?;
                                }
                                Ok(())
                            },
                        )
                    })
                });

                // Write packed chunks. The module refuses a label that is not
                // uninitialized, and a hash that is no longer this assignment.
                let mut written: u32 = 0;
                while let Some(chunk) = rx.recv().await {
                    let i = written;
                    written += 1;
                    let offset = PartitionChunkOffset::from(start + i);
                    let wrote = storage_module_clone.write_entropy_chunk(
                        offset,
                        chunk,
                        partition_hash,
                    );
                    match wrote {
                        EntropyWrite::Rejected(EntropyRejection::NotUninitialized(_)) => continue,
                        EntropyWrite::Rejected(EntropyRejection::NotAssigned) => {
                            stop_flag.store(true, Ordering::Relaxed);
                            return Err(format!(
                                "entropy write refused at offset {} during CUDA packing for SM {}",
                                start + i,
                                storage_module_id
                            ));
                        }
                        EntropyWrite::Queued => {}
                    }

                    // Wait for the disk before queueing more entropy.
                    if i.is_multiple_of(short_writes_before_sync) {
                        yield_now().await;
                        if let Err(err) = crate::packing_service::wait_for_pending_room(
                            &storage_module_clone,
                            "CUDA packing (periodic)",
                        )
                        .await
                        {
                            stop_flag.store(true, Ordering::Relaxed);
                            return Err(err);
                        }
                    }
                }

                // The channel closed: either every chunk arrived, or the GPU side failed.
                match gpu.await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        stop_flag.store(true, Ordering::Relaxed);
                        return Err(format!(
                            "CUDA packing failed for SM {storage_module_id}: {err:?}"
                        ));
                    }
                    Err(err) => {
                        stop_flag.store(true, Ordering::Relaxed);
                        return Err(format!(
                            "CUDA packing task panicked for SM {storage_module_id}: {err:?}"
                        ));
                    }
                }
                if written != num_chunks {
                    stop_flag.store(true, Ordering::Relaxed);
                    return Err(format!(
                        "CUDA packing produced {written} of {num_chunks} chunks for SM {storage_module_id}"
                    ));
                }
                drop(permit);
                Ok(())
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

        // Wait for all packing tasks to complete. A refused write or a
        // panicked worker must fail the range so the caller can requeue it.
        let mut failed = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!(
                        target: "irys::packing",
                        "CUDA packing task failed for SM {}: {}", storage_module_id, err
                    );
                    failed.get_or_insert(err);
                }
                Err(err) => {
                    warn!(
                        target: "irys::packing",
                        "CUDA packing task failed for SM {}: {:?}", storage_module_id, err
                    );
                    failed.get_or_insert(format!(
                        "CUDA packing task failed for SM {storage_module_id}: {err:?}"
                    ));
                }
            }
        }
        if let Some(err) = failed {
            return Err(err);
        }

        Ok(())
    }
}
