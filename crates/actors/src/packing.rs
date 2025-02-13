use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
    time::Duration,
};

use actix::{Actor, Addr, Context, Handler, Message, MessageResponse};

use eyre::eyre;
use irys_packing::{capacity_single::compute_entropy_chunk, PackingType, PACKING_TYPE};

#[cfg(feature = "nvidia")]
use {
    irys_packing::capacity_pack_range_cuda_c,
    irys_types::{split_interval, CHUNK_SIZE},
};

use irys_storage::{ChunkType, InclusiveInterval, StorageModule};
use irys_types::{PartitionChunkOffset, PartitionChunkRange, StorageConfig};
use reth::tasks::TaskExecutor;
use tokio::{runtime::Handle, sync::Semaphore, time::sleep};
use tracing::{debug, warn};

#[derive(Debug, Message, Clone)]
#[rtype("()")]
pub struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

pub type AtomicPackingJobQueue = Arc<RwLock<VecDeque<PackingRequest>>>;
pub type PackingJobsBySM = HashMap<usize, AtomicPackingJobQueue>;

pub type PackingSemaphores = HashMap<usize, Arc<Semaphore>>;

#[derive(Debug, Clone)]
/// Packing actor state
pub struct PackingActor {
    /// used to execute the internal job poll future
    actix_runtime_handle: Handle,
    /// used to spawn threads to perform packing
    task_executor: TaskExecutor,
    /// list of all the pending packing jobs
    pending_jobs: PackingJobsBySM,
    /// semaphore to control concurrency -- sm_id => semaphore
    semaphore: PackingSemaphores,
    /// packing process configuration
    config: PackingConfig,
}

#[derive(Debug, Clone)]
/// configuration for the packing actor
pub struct PackingConfig {
    pub poll_duration: Duration,
    /// Max. number of packing threads for CPU packing
    pub concurrency: u16,
    /// Max. number of chunks send to GPU packing
    #[allow(unused)]
    pub max_chunks: u32,
}
impl Default for PackingConfig {
    fn default() -> Self {
        Self {
            poll_duration: Duration::from_millis(1000),
            concurrency: 4,
            max_chunks: 1024,
        }
    }
}

impl PackingActor {
    /// creates a new packing actor
    pub fn new(
        actix_runtime_handle: Handle,
        task_executor: TaskExecutor,
        storage_module_ids: Vec<usize>,
        config: Option<PackingConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let semaphore = storage_module_ids
            .iter()
            .map(|s| (*s, Arc::new(Semaphore::new(config.concurrency.into()))))
            .collect();
        let pending_jobs = storage_module_ids
            .iter()
            .map(|s| (*s, Arc::new(RwLock::new(VecDeque::with_capacity(32)))))
            .collect();

        Self {
            actix_runtime_handle,
            task_executor,
            pending_jobs,
            semaphore,
            config,
        }
    }

    async fn process_jobs(self, _storage_module_id: usize, pending_jobs: AtomicPackingJobQueue) {
        loop {
            // block as the compiler can't reason about explicit read guard drops with Send bounds apparently
            let front = {
                let pending_read_guard = pending_jobs.read().unwrap();
                pending_read_guard.front().cloned()
            };

            let next_range = match front {
                Some(v) => v,
                None => {
                    debug!("no packing requests in queue, sleeping...");
                    tokio::time::sleep(self.config.poll_duration).await;
                    continue;
                }
            };

            // TODO: should we be validating if the requested range is valid?
            let PackingRequest {
                storage_module,
                chunk_range,
            } = next_range;

            let assignment = match storage_module.partition_assignment {
                Some(v) => v,
                None => {
                    warn!(target:"irys::packing", "Partition assignment for storage module {} is `None`, cannot pack requested range {:?}", &storage_module.id, &chunk_range);
                    pending_jobs.write().unwrap().pop_front();
                    continue;
                }
            };

            let mining_address = assignment.miner_address;
            let partition_hash = assignment.partition_hash;
            let StorageConfig {
                chunk_size,
                entropy_packing_iterations,
                ..
            } = storage_module.storage_config;
            let storage_module_id = storage_module.id;
            let semaphore = self.semaphore.get(&storage_module_id).unwrap();

            let start_value = *chunk_range.0.start();
            let end_value = *chunk_range.0.end();

            match PACKING_TYPE {
                PackingType::CPU => {
                    for i in start_value..=end_value {
                        // each semaphore permit corresponds to a single chunk to be packed, as we assume it'll use an entire CPU thread's worth of compute.
                        // when we implement GPU packing, this is where we need to fork the logic between the two methods - GPU can take larger contiguous segments
                        // whereas CPU will do this permit system

                        // TODO: have stateful executor threads / an arena for entropy chunks so we don't have to allocate chunks all over the place when we can just re-use
                        // TODO: improve this! use wakers instead of polling, allow for work-stealing, use a dedicated thread pool w/ lower priorities etc.
                        let storage_module = storage_module.clone();
                        let semaphore = semaphore.clone();
                        // wait for the permit before spawning the thread
                        let permit = semaphore.acquire_owned().await.unwrap();
                        //debug!(target: "irys::packing", "Packing chunk {} for SM {} partition_hash {} mining_address {} iterations {}", &i, &storage_module.id, &partition_hash, &mining_address, &entropy_packing_iterations);
                        self.task_executor.spawn_blocking(async move {
                            let mut out = Vec::with_capacity(chunk_size as usize);
                            compute_entropy_chunk(
                                mining_address,
                                i as u64,
                                partition_hash.0,
                                entropy_packing_iterations,
                                chunk_size as usize,
                                &mut out,
                            );

                            debug!(target: "irys::packing::progress", "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {} iterations {}", &i, &storage_module_id, &partition_hash, &mining_address, &entropy_packing_iterations);
                            // write the chunk
                            //debug!(target: "irys::packing", "Writing chunk range {} to SM {}", &i, &storage_module.id);
                            storage_module.write_chunk(PartitionChunkOffset::from(i), out, ChunkType::Entropy);
                            let _ = storage_module.sync_pending_chunks();
                            drop(permit); // drop after chunk write so the SM can apply backpressure to packing
                        });

                        if i % 1000 == 0 {
                            debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {} iterations {}", chunk_range.0.start(), &i, chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &entropy_packing_iterations);
                        }
                    }
                    debug!(target: "irys::packing::done", "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {} iterations {}",  chunk_range.0.start(),chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &entropy_packing_iterations);
                }
                #[cfg(feature = "nvidia")]
                PackingType::CUDA => {
                    assert_eq!(
                        chunk_size, CHUNK_SIZE,
                        "Chunk size is not aligned with C code"
                    );
                    for chunk_range_split in split_interval(&chunk_range, self.config.max_chunks)
                        .unwrap()
                        .iter()
                    {
                        debug!(
                            "Packing using CUDA C implementation, start:{} end:{}!",
                            chunk_range_split.start(),
                            chunk_range_split.end()
                        );
                        let num_chunks: u32 =
                            (chunk_range_split.end() as u32 - chunk_range_split.start() as u32 + 1);
                        let storage_module = storage_module.clone();
                        let chunk_range_split = chunk_range_split.clone();

                        let semaphore = self.semaphore.get(&storage_module_id).unwrap().clone();
                        // wait for the permit before spawning the thread
                        let permit = semaphore.acquire_owned().await.unwrap();

                        self.task_executor.spawn_blocking(async move {
                            let mut out: Vec<u8> = Vec::with_capacity(
                                (num_chunks * chunk_size as u32).try_into().unwrap(),
                            );

                            capacity_pack_range_cuda_c(
                                num_chunks,
                                mining_address,
                                chunk_range_split.start() as u64,
                                partition_hash,
                                Some(entropy_packing_iterations),
                                &mut out,
                            );
                            for i in 0..num_chunks {
                                storage_module.write_chunk(
                                    chunk_range_split.start() + i,
                                    out[(i * chunk_size as u32) as usize
                                        ..((i + 1) * chunk_size as u32) as usize]
                                        .to_vec(),
                                    ChunkType::Entropy,
                                );
                                let _ = storage_module.sync_pending_chunks();
                            }
                            drop(permit); // drop after chunk write so the SM can apply backpressure to packing
                        });
                        debug!(target: "irys::packing::update", "CUDA Packed chunks {} - {} for SM {} partition_hash {} mining_address {} iterations {}", chunk_range_split.start(), chunk_range_split.end(), &storage_module_id, &partition_hash, &mining_address, &entropy_packing_iterations);
                    }
                }
                _ => unimplemented!(),
            }

            // Remove from queue once complete
            let _ = pending_jobs.write().unwrap().pop_front();
        }
    }
}

#[cfg(test)]
#[inline]
fn cast_vec_u8_to_vec_u8_array<const N: usize>(input: Vec<u8>) -> Vec<[u8; N]> {
    assert!(input.len() % N == 0, "wrong input N {}", N);
    let length = input.len() / N;
    let ptr = input.as_ptr() as *const [u8; N];
    std::mem::forget(input); // So input never drops

    // safety: we've asserted that `input` length is divisible by N
    unsafe { Vec::from_raw_parts(ptr as *mut [u8; N], length, length) }
}

impl Actor for PackingActor {
    type Context = Context<Self>;

    fn start(self) -> actix::Addr<Self> {
        let keys = self.pending_jobs.keys().cloned().collect::<Vec<usize>>();
        for key in keys {
            self.actix_runtime_handle.spawn(Self::process_jobs(
                self.clone(),
                key,
                self.pending_jobs.get(&key).unwrap().clone(),
            ));
        }

        Context::new().run(self)
    }

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(5_000);
    }
}

impl Handler<PackingRequest> for PackingActor {
    type Result = ();

    fn handle(&mut self, msg: PackingRequest, _ctx: &mut Self::Context) -> Self::Result {
        debug!(target: "irys::packing", "Received packing request for range {}-{} for SM {}", &msg.chunk_range.0.start(), &msg.chunk_range.0.end(), &msg.storage_module.id);
        self.pending_jobs
            .get(&msg.storage_module.id)
            .unwrap()
            .as_ref()
            .write()
            .unwrap()
            .push_back(msg);
    }
}

#[derive(Debug, Message, Clone)]
#[rtype("Internals")]
pub struct GetInternals();

#[derive(Debug, MessageResponse, Clone)]
pub struct Internals {
    pending_jobs: PackingJobsBySM,
    semaphore: PackingSemaphores,
    config: PackingConfig,
}

impl Handler<GetInternals> for PackingActor {
    type Result = Internals;

    fn handle(&mut self, _msg: GetInternals, _ctx: &mut Self::Context) -> Self::Result {
        Internals {
            pending_jobs: self.pending_jobs.clone(),
            semaphore: self.semaphore.clone(),
            config: self.config.clone(),
        }
    }
}

/// waits for any pending & active packing tasks to complete
pub async fn wait_for_packing(
    packing_addr: Addr<PackingActor>,
    timeout: Option<Duration>,
) -> eyre::Result<()> {
    let internals = packing_addr.send(GetInternals()).await?;
    tokio::time::timeout(timeout.unwrap_or(Duration::from_secs(10)), async {
        loop {
            // Counts all jobs in all job queues
            if internals
                .pending_jobs
                .values()
                .fold(0, |acc, x| acc + x.as_ref().read().unwrap().len())
                == 0
            {
                // try to get all the semaphore permits - this is how we know that the packing is done
                let _permit =
                    futures::future::join_all(internals.semaphore.iter().map(|(_, s)| {
                        s.as_ref().acquire_many(internals.config.concurrency as u32)
                    }))
                    .await
                    .iter()
                    .map(|r| r.as_ref().unwrap());
                break Some(());
            } else {
                sleep(Duration::from_millis(100)).await
            }
        }
    })
    .await?
    .ok_or_else(|| eyre!("timed out waiting for packing to complete"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use actix::Actor as _;
    use irys_packing::capacity_single::compute_entropy_chunk;
    use irys_storage::{ii, ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        partition::{PartitionAssignment, PartitionHash},
        partition_chunk_offset_ii, Address, PartitionChunkOffset, PartitionChunkRange,
        StorageConfig,
    };
    use reth::tasks::TaskManager;
    use tokio::runtime::Handle;

    use crate::packing::{
        cast_vec_u8_to_vec_u8_array, wait_for_packing, PackingActor, PackingRequest,
    };

    #[actix::test]
    async fn test_packing_actor() -> eyre::Result<()> {
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
            submodules: vec![
                (partition_chunk_offset_ii!(0, 4), "hdd0-4TB".into()), // 0 to 4 inclusive
            ],
        }];

        // Override the default StorageModule config for testing
        let storage_config = StorageConfig {
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1_000,
            num_chunks_in_partition: 5,
            ..Default::default()
        };

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_packing_actor"), false);
        let base_path = tmp_dir.path().to_path_buf();

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = Arc::new(StorageModule::new(
            &base_path,
            storage_module_info,
            storage_config.clone(),
        )?);

        let request = PackingRequest {
            storage_module: storage_module.clone(),
            chunk_range: PartitionChunkRange(partition_chunk_offset_ii!(0, 3)),
        };
        // Create an instance of the mempool actor
        let task_manager = TaskManager::current();

        let sm_ids = vec![storage_module.id];

        let packing = PackingActor::new(Handle::current(), task_manager.executor(), sm_ids, None);

        let packing_addr = packing.start();

        packing_addr.send(request).await?;

        wait_for_packing(packing_addr, None).await?;

        storage_module.sync_pending_chunks()?;
        // check that the chunks are marked as packed
        let intervals = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(
            intervals,
            vec![ii(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(3)
            )]
        );
        let stored_entropy = storage_module.read_chunks(ii(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(3),
        ))?;
        // verify the packing
        for i in 0..=3 {
            let chunk = stored_entropy.get(&PartitionChunkOffset::from(i)).unwrap();

            let mut out = Vec::with_capacity(storage_config.chunk_size as usize);
            compute_entropy_chunk(
                mining_address,
                i as u64,
                partition_hash.0,
                storage_config.entropy_packing_iterations,
                storage_config.chunk_size.try_into().unwrap(),
                &mut out,
            );
            assert_eq!(chunk.0.first(), out.first());
        }

        Ok(())
    }

    #[test]
    fn test_casting() {
        let v: Vec<u8> = (1..=9).collect();
        let c2 = cast_vec_u8_to_vec_u8_array::<3>(v);

        assert_eq!(c2, vec![[1, 2, 3], [4, 5, 6], [7, 8, 9]])
    }

    #[test]
    #[should_panic(expected = "wrong input N 3")]
    fn test_casting_error() {
        let v: Vec<u8> = (1..=10).collect();
        let c2 = cast_vec_u8_to_vec_u8_array::<3>(v);
    }
}
