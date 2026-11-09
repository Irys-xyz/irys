use futures::{
    StreamExt as _,
    stream::{self, BoxStream},
};
use irys_packing::PackingType;
use tracing::trace;

#[cfg(feature = "nvidia")]
use irys_packing::CUDAConfig;
#[cfg(feature = "nvidia")]
use irys_types::split_interval;

use irys_packing::capacity_single::SHA_HASH_SIZE;
use irys_types::remote_packing::REMOTE_PACK_WINDOW_CHUNKS;

use crate::types::RemotePackingRequest;
use crate::worker::{PACKING_TYPE, PackingWorkerState};

/// Rejects requests with an unsupported chunk size or a range wider than one node window,
/// before the worker allocates for them. Does not bound `entropy_packing_iterations`.
///
/// The CUDA kernel needs the protocol chunk size. The CPU path builds entropy from whole
/// SHA-256 outputs, so it needs a multiple of `SHA_HASH_SIZE` in `SHA_HASH_SIZE..=CHUNK_SIZE`.
pub fn validate_request(request: &RemotePackingRequest) -> Result<(), String> {
    let max_chunk_size = irys_types::ConsensusConfig::CHUNK_SIZE;
    let hash_size = SHA_HASH_SIZE as u64;
    let chunk_size_supported = match PACKING_TYPE {
        PackingType::CUDA => request.chunk_size == max_chunk_size,
        _ => {
            (hash_size..=max_chunk_size).contains(&request.chunk_size)
                && request.chunk_size.is_multiple_of(hash_size)
        }
    };
    if !chunk_size_supported {
        return Err(format!(
            "unsupported chunk_size {}, expected {} for {PACKING_TYPE:?} packing",
            request.chunk_size,
            if matches!(PACKING_TYPE, PackingType::CUDA) {
                format!("{max_chunk_size}")
            } else {
                format!("a multiple of {hash_size} in {hash_size}..={max_chunk_size}")
            }
        ));
    }
    let start = u64::from(*request.chunk_range.0.start());
    let end = u64::from(*request.chunk_range.0.end());
    if end < start || end - start + 1 > u64::from(REMOTE_PACK_WINDOW_CHUNKS) {
        return Err(format!(
            "chunk range {start}..={end} is empty or wider than {REMOTE_PACK_WINDOW_CHUNKS} chunks"
        ));
    }
    Ok(())
}

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
                let chunk_size_usize =
                    usize::try_from(chunk_size).expect("protocol-fixed chunk_size fits usize");
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

                        let result = runtime_handle.spawn_blocking(move || {
                            let mut out = Vec::with_capacity(chunk_size_usize);
                            irys_packing::capacity_single::compute_entropy_chunk(
                                mining_address,
                                u64::from(i),
                                partition_hash.0,
                                entropy_packing_iterations,
                                chunk_size_usize,
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
                .buffered(usize::from(cpu_packing_concurrency))
                .inspect(move |_| {
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
                // the worker runs one GPU batch at a time (see `start_worker`)
                let batch_size = match self.0.config.gpu_packing_batch_size {
                    irys_types::config::GpuPackingBatchSize::Fixed(chunks) => chunks,
                    irys_types::config::GpuPackingBatchSize::Auto => {
                        CUDAConfig::from_device_default()?.auto_batch_size_for_device(0, 1)?
                    }
                };
                let split = split_interval(&chunk_range, batch_size)?;
                let packers = self.0.cuda_packers.clone();
                // Slabs stream to the client as the GPU produces them; two in flight (256 MiB),
                // so a slow client holds the GPU back instead of growing host memory.
                let (tx, rx) = tokio::sync::mpsc::channel::<eyre::Result<Vec<u8>>>(2);
                runtime_handle.clone().spawn(async move {
                    for chunk_range_split in split {
                        let start: u32 = *chunk_range_split.start();
                        let end: u32 = *chunk_range_split.end();
                        let num_chunks = end - start + 1;
                        tracing::debug!(
                            "Packing using CUDA, start:{} end:{} (len: {})",
                            &start,
                            &end,
                            &num_chunks
                        );
                        let permit = match semaphore.clone().acquire_owned().await {
                            Ok(permit) => permit,
                            Err(err) => {
                                let _ = tx.send(Err(err.into())).await;
                                return;
                            }
                        };
                        let (gpu_tx, packers) = (tx.clone(), packers.clone());
                        let packed = runtime_handle
                            .spawn_blocking(move || {
                                packers.with_packer(batch_size, |packer| {
                                    packer.pack(
                                        num_chunks,
                                        mining_address,
                                        u64::from(start),
                                        partition_hash,
                                        entropy_packing_iterations,
                                        chain_id,
                                        |_first, slab| {
                                            gpu_tx
                                                .blocking_send(Ok(slab.to_vec()))
                                                .map_err(|_| eyre::eyre!("client went away"))
                                        },
                                    )
                                })
                            })
                            .await;
                        drop(permit);
                        let failed = match packed {
                            Ok(Ok(())) => None,
                            Ok(Err(err)) => Some(err),
                            Err(err) => Some(eyre::eyre!("CUDA packing task failed: {err}")),
                        };
                        if let Some(err) = failed {
                            let _ = tx.send(Err(err)).await;
                            return;
                        }
                        tracing::debug!(
                            target = "irys::packing::update",
                            packing_job.chunk_range = ?chunk_range,
                            packing_job.partition_hash = ?partition_hash,
                            packing_job.mining_address = ?mining_address,
                            packing_job.entropy_packing_iterations = ?entropy_packing_iterations,
                            "CUDA Packed chunks"
                        );
                    }
                });
                let stream = stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                })
                .boxed();
                Ok(stream)
            }
            other => eyre::bail!("unsupported packing type: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{
        H256, IrysAddress, PartitionChunkOffset, PartitionChunkRange, ii, partition_chunk_offset_ii,
    };

    fn request(chunk_size: u64, start: u32, end: u32) -> RemotePackingRequest {
        RemotePackingRequest {
            mining_address: IrysAddress::ZERO,
            partition_hash: H256::zero(),
            chunk_range: PartitionChunkRange(partition_chunk_offset_ii!(start, end)),
            chain_id: 1,
            chunk_size,
            entropy_packing_iterations: 1,
        }
    }

    #[test]
    fn validate_request_accepts_node_window() {
        let chunk_size = irys_types::ConsensusConfig::CHUNK_SIZE;
        assert!(validate_request(&request(chunk_size, 0, REMOTE_PACK_WINDOW_CHUNKS - 1)).is_ok());
    }

    #[cfg(not(feature = "nvidia"))]
    #[test]
    fn validate_request_accepts_small_chunk_size_on_cpu() {
        assert!(validate_request(&request(32, 0, 10)).is_ok());
        assert!(validate_request(&request(64, 0, 10)).is_ok());
    }

    #[cfg(not(feature = "nvidia"))]
    #[test]
    fn validate_request_rejects_chunk_size_not_a_multiple_of_the_hash_size_on_cpu() {
        assert!(validate_request(&request(1, 0, 10)).is_err());
        assert!(validate_request(&request(31, 0, 10)).is_err());
        assert!(validate_request(&request(100, 0, 10)).is_err());
    }

    #[test]
    fn validate_request_rejects_zero_or_oversized_chunk_size() {
        let max = irys_types::ConsensusConfig::CHUNK_SIZE;
        assert!(validate_request(&request(0, 0, 10)).is_err());
        assert!(validate_request(&request(max + 1, 0, 10)).is_err());
        assert!(validate_request(&request(1 << 40, 0, 10)).is_err());
    }

    #[test]
    fn validate_request_rejects_wide_range() {
        let chunk_size = irys_types::ConsensusConfig::CHUNK_SIZE;
        assert!(validate_request(&request(chunk_size, 0, REMOTE_PACK_WINDOW_CHUNKS)).is_err());
        assert!(validate_request(&request(chunk_size, 0, u32::MAX)).is_err());
    }
}
