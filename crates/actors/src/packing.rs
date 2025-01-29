use std::{sync::Arc, time::Duration};

use eyre::eyre;
use irys_packing::{capacity_single::compute_entropy_chunk, PackingType, PACKING_TYPE};

#[cfg(feature = "nvidia")]
use {
    irys_packing::capacity_pack_range_cuda_c,
    irys_types::{split_interval, CHUNK_SIZE},
};

use irys_storage::{ChunkType, StorageModule};
use irys_types::{PartitionChunkRange, StorageConfig};
use tokio::{
    sync::{mpsc, Semaphore},
    task::spawn_blocking,
    time::sleep,
};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

#[derive(Debug, Clone)]
pub struct PackingConfig {
    pub poll_duration: Duration,
    pub concurrency: usize,
    pub max_chunks: u32,
    pub num_storage_modules: usize,
}

impl Default for PackingConfig {
    fn default() -> Self {
        Self {
            poll_duration: Duration::from_millis(1000),
            concurrency: 4,
            max_chunks: 1024,
            num_storage_modules: 4,
        }
    }
}

pub struct PackingService {
    sender: mpsc::Sender<PackingRequest>,
    semaphore: Arc<Semaphore>,
    max_permits: usize, // Added manual tracking for max permits
}

impl PackingService {
    pub fn new(config: PackingConfig) -> Self {
        let (sender, receiver) = mpsc::channel(config.num_storage_modules);
        let semaphore = Arc::new(Semaphore::new(config.concurrency));
        let max_permits = config.concurrency; // Store the max permits manually

        let worker = Worker {
            receiver,
            semaphore: semaphore.clone(),
            config,
        };

        tokio::spawn(worker.run());

        Self {
            sender,
            semaphore,
            max_permits,
        }
    }

    /// **Centralized entrypoint function** to submit work from any thread.
    pub fn submit(
        &self,
        storage_module: Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
    ) -> eyre::Result<()> {
        let job = PackingRequest {
            storage_module,
            chunk_range,
        };
        let sender = self.sender.clone();

        tokio::spawn(async move {
            if sender.send(job).await.is_err() {
                warn!("Packing request queue is full, dropping job.");
            }
        });

        Ok(())
    }

    pub async fn wait_for_completion(&self, timeout: Option<Duration>) -> eyre::Result<()> {
        let timeout = timeout.unwrap_or(Duration::from_secs(10));
        tokio::time::timeout(timeout, async {
            loop {
                if self.semaphore.available_permits() == self.max_permits {
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| eyre!("Timed out waiting for packing to complete"))
    }
}

struct Worker {
    receiver: mpsc::Receiver<PackingRequest>,
    semaphore: Arc<Semaphore>,
    config: PackingConfig,
}

impl Worker {
    async fn run(mut self) {
        while let Some(request) = self.receiver.recv().await {
            let semaphore = self.semaphore.clone();
            let permit = semaphore.acquire_owned().await.unwrap();
            let config = self.config.clone();

            tokio::spawn(async move {
                Self::process_request(request, config).await;
                drop(permit);
            });
        }
    }

    async fn process_request(request: PackingRequest, config: PackingConfig) {
        let PackingRequest {
            storage_module,
            chunk_range,
        } = request;

        let assignment = match storage_module.partition_assignment {
            Some(v) => v,
            None => {
                warn!(
                    "Partition assignment for storage module {} is None, skipping",
                    &storage_module.id
                );
                return;
            }
        };

        let mining_address = assignment.miner_address;
        let partition_hash = assignment.partition_hash;
        let StorageConfig {
            chunk_size,
            entropy_packing_iterations,
            ..
        } = storage_module.storage_config;

        match PACKING_TYPE {
            PackingType::CPU => {
                for i in chunk_range.start()..=chunk_range.end() {
                    let storage_module_clone = Arc::clone(&storage_module); // Fix: Clone before move

                    spawn_blocking(move || {
                        let mut out = vec![0; chunk_size as usize];
                        compute_entropy_chunk(
                            mining_address,
                            i as u64,
                            partition_hash.0,
                            entropy_packing_iterations,
                            chunk_size as usize,
                            &mut out,
                        );

                        storage_module_clone.write_chunk(i, out, ChunkType::Entropy);
                        storage_module_clone.sync_pending_chunks().ok();
                    });

                    if i % 1000 == 0 {
                        debug!(
                            "CPU Packed chunks {} - {} for SM {}",
                            chunk_range.start(),
                            i,
                            storage_module.id
                        );
                    }
                }
            }
            #[cfg(feature = "nvidia")]
            PackingType::CUDA => {
                assert_eq!(chunk_size, CHUNK_SIZE, "Chunk size mismatch");
                for chunk_range_split in split_interval(&chunk_range, config.max_chunks).unwrap() {
                    let num_chunks = chunk_range_split.end() - chunk_range_split.start() + 1;
                    let storage_module_clone = Arc::clone(&storage_module); // Fix: Clone before move

                    spawn_blocking(move || {
                        let mut out = vec![0; (num_chunks * chunk_size as u32) as usize];
                        capacity_pack_range_cuda_c(
                            num_chunks,
                            mining_address,
                            chunk_range_split.start() as u64,
                            partition_hash,
                            Some(entropy_packing_iterations),
                            &mut out,
                        );

                        for i in 0..num_chunks {
                            let offset = (i * chunk_size as u32) as usize;
                            storage_module_clone.write_chunk(
                                chunk_range_split.start() + i,
                                out[offset..offset + chunk_size as usize].to_vec(),
                                ChunkType::Entropy,
                            );
                            storage_module_clone.sync_pending_chunks().ok();
                        }
                    });
                }
            }
            _ => unimplemented!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_storage::{ii, initialize_storage_files, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        partition::{PartitionAssignment, PartitionHash},
        Address, StorageConfig,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn test_packing_service() -> eyre::Result<()> {
        let mining_address = Address::random();
        let partition_hash = PartitionHash::zero();
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: mining_address,
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(ii(0, 4), "hdd0-4TB".into())],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_packing_service"), false);
        let base_path = tmp_dir.path().to_path_buf();
        initialize_storage_files(&base_path, &infos, &vec![])?;

        let storage_config = StorageConfig {
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1_000,
            num_chunks_in_partition: 5,
            ..Default::default()
        };

        let storage_module = Arc::new(StorageModule::new(
            &base_path,
            &infos[0],
            storage_config.clone(),
        )?);

        let service = PackingService::new(PackingConfig {
            num_storage_modules: infos.len(),
            ..Default::default()
        });

        service.submit(storage_module.clone(), ii(0, 3).into())?;

        service.wait_for_completion(None).await?;

        let intervals = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(intervals, vec![ii(0, 3)]);

        Ok(())
    }
}
