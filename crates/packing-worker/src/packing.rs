use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use irys_packing::PackingType;
use tracing::trace;

#[cfg(feature = "nvidia")]
use irys_packing::CUDAConfig;
#[cfg(feature = "nvidia")]
use irys_packing::capacity_pack_range_cuda_c;
#[cfg(feature = "nvidia")]
use irys_types::split_interval;

use crate::types::RemotePackingRequest;
use crate::worker::{PACKING_TYPE, PackingWorkerState};

impl PackingWorkerState {
    pub fn pack(
        &self,
        job: RemotePackingRequest,
    ) -> eyre::Result<BoxStream<'static, Result<Vec<u8>, eyre::Report>>> {
        let RemotePackingRequest {
            mining_address,
            partition_hash,
            chunk_range,
            chain_id,
            chunk_size,
            entropy_packing_iterations,
        } = job;

        let start_value = *chunk_range.0.start();
        let end_value = *chunk_range.0.end();
        let runtime_handle = self.0.runtime_handle.clone();
        let semaphore = self.0.packing_semaphore.clone();

        match PACKING_TYPE {
            PackingType::CPU => {
                let cpu_packing_concurrency = self.0.config.cpu_packing_concurrency;
                let stream = stream::iter(start_value..=end_value).map(move  |i| {
                    let runtime_handle = runtime_handle.clone();
                    let semaphore = semaphore.clone();
                    async move {
                        let _permit = semaphore.acquire_owned().await?;

                        if i % 1000 == 0 {
                            trace!(
                                target: "irys::packing::done",
                                "CPU Packed chunks {} / {} for partition_hash {:?} mining_address {:?} iterations {}",
                                i, end_value,  partition_hash, mining_address, entropy_packing_iterations
                            );
                        }

                        // Spawn blocking task
                        let result = runtime_handle.spawn_blocking(move || {
                            let mut out = Vec::with_capacity(chunk_size as usize);
                            irys_packing::capacity_single::compute_entropy_chunk(
                                mining_address,
                                i as u64,
                                partition_hash.0,
                                entropy_packing_iterations,
                                chunk_size as usize,
                                &mut out,
                                chain_id,
                            );
                            out
                        }).await?;

                        Ok(result)
                    }
                    }
                )
                // note: `buffered` both spawns multiple futures (up to `cpu_packing_concurrency`) and ensures they yield in the correct order
                .buffered(cpu_packing_concurrency as usize)
                .inspect(move |_| {
                    // This runs after all tasks complete
                    trace!(
                        target: "irys::packing::done",
                        "CPU Packed chunks complete for  partition_hash {:?} mining_address {:?} iterations {}",
                        partition_hash, mining_address, entropy_packing_iterations
                    );
                })
                .boxed();

                Ok(stream)
            }

            #[cfg(feature = "nvidia")]
            PackingType::CUDA => {
                assert_eq!(
                    chunk_size,
                    irys_types::ConsensusConfig::CHUNK_SIZE,
                    "Chunk size is not aligned with C code"
                );
                let split = split_interval(&chunk_range, self.0.config.gpu_packing_batch_size)?;
                let stream = stream::iter(split).map(move |chunk_range_split| {
                    let runtime_handle = runtime_handle.clone();
                    let semaphore = semaphore.clone();
                    async move {
                        let _permit = semaphore.acquire_owned().await?;
                        let start: u32 = *(*chunk_range_split).start();
                        let end: u32 = *(*chunk_range_split).end();

                        let num_chunks = end - start + 1;

                        tracing::debug!(
                            "Packing using CUDA C implementation, start:{} end:{} (len: {})",
                            &start, &end, &num_chunks
                        );


                        let out = runtime_handle
                            .spawn_blocking(move || {

                                let mut out: Vec<u8> = Vec::with_capacity(
                                    (num_chunks * chunk_size as u32).try_into().unwrap(),
                                );
                                capacity_pack_range_cuda_c(
                                    num_chunks,
                                    mining_address,
                                    start as u64,
                                    partition_hash,
                                    entropy_packing_iterations,
                                    chain_id,
                                    CUDAConfig::from_device_default().unwrap(),
                                    &mut out,
                                ).unwrap();
                                out
                            })
                            .await?;
                        tracing::debug!(
                            target = "irys::packing::update",
                            packing_job.chunk_range = ?chunk_range,
                            packing_job.partition_hash = ?partition_hash,
                            packing_job.mining_address = ?mining_address,
                            packing_job.entropy_packing_iterations = ?entropy_packing_iterations,
                            "CUDA Packed chunks"
                        );
                        Ok(out)
                    }}).buffered(1).boxed();
                Ok(stream)
            }
            _ => unimplemented!(),
        }
    }
}
