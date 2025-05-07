use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, RwLock},
    time::Duration,
};

use actix::{Actor, Addr, Context, Handler, Message, MessageResponse};

use eyre::eyre;
use irys_packing::{capacity_single::compute_entropy_chunk, PackingType, PACKING_TYPE};

#[cfg(feature = "nvidia")]
use {irys_packing::capacity_pack_range_cuda_c, irys_types::split_interval};

use irys_storage::{ChunkType, StorageModule};
use irys_types::{Config, PartitionChunkOffset, PartitionChunkRange};
use reth::tasks::TaskExecutor;
use tokio::{sync::Semaphore, time::sleep};
use tracing::{debug, warn};

#[derive(Debug, Message, Clone)]
#[rtype("()")]
pub struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

pub type AtomicPackingJobQueue = Arc<RwLock<VecDeque<PackingRequest>>>;
pub type PackingJobsBySM = HashMap<usize, AtomicPackingJobQueue>;

pub type PackingSemaphore = Arc<Semaphore>;

#[derive(Debug, Clone)]
/// Packing actor state
pub struct PackingActor {
    /// used to spawn threads to perform packing
    task_executor: TaskExecutor,
    /// list of all the pending packing jobs
    pending_jobs: PackingJobsBySM,
    /// semaphore to control concurrency -- sm_id => semaphore
    semaphore: PackingSemaphore,
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
    #[cfg(feature = "nvidia")]
    pub max_chunks: u32,
    /// Irys chain id
    pub chain_id: u64,
}

impl PackingConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            poll_duration: Duration::from_millis(1000),
            concurrency: config.node_config.packing.cpu_packing_concurrency,
            chain_id: config.consensus.chain_id,
            #[cfg(feature = "nvidia")]
            max_chunks: config.node_config.packing.gpu_packing_batch_size,
        }
    }
}

impl PackingActor {
    /// creates a new packing actor
    pub fn new(
        task_executor: TaskExecutor,
        storage_module_ids: Vec<usize>,
        config: PackingConfig,
    ) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.concurrency.into()));
        let pending_jobs = storage_module_ids
            .iter()
            .map(|s| (*s, Arc::new(RwLock::new(VecDeque::with_capacity(32)))))
            .collect();

        Self {
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
                    // debug!("no packing requests in queue, sleeping...");
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
            let storage_module_id = storage_module.id;
            let semaphore = self.semaphore.clone();

            let start_value = *chunk_range.0.start();
            let end_value = *chunk_range.0.end();
            let short_writes_before_sync: u32 = (storage_module
                .config
                .node_config
                .storage
                .num_writes_before_sync
                .div_ceil(2))
            .try_into()
            .expect("Should be able to convert min_writes_before_sync to u32");
            match PACKING_TYPE {
                PackingType::CPU => {
                    for i in start_value..=end_value {
                        // each semaphore permit corresponds to a single chunk to be packed, as we assume it'll use an entire CPU thread's worth of compute.
                        // when we implement GPU packing, this is where we need to fork the logic between the two methods - GPU can take larger contiguous segments
                        // whereas CPU will do this permit system

                        // TODO: have stateful executor threads / an arena for entropy chunks so we don't have to allocate chunks all over the place when we can just re-use
                        // TODO: improve this! use wakers instead of polling, allow for work-stealing, use a dedicated thread pool w/ lower priorities etc
                        if i % short_writes_before_sync == 0 {
                            debug!("triggering sync");
                            let _ = storage_module.sync_pending_chunks();
                        }

                        // wait for the permit before spawning the thread

                        // debug!(target: "irys::packing", "Packing chunk {} for SM {} partition_hash {} mining_address {} iterations {}", &i, &storage_module.id, &partition_hash, &mining_address, &entropy_packing_iterations);
                        let config = storage_module.config.clone();
                        self.task_executor.spawn_critical_blocking("packing worker", {
                            let storage_module = storage_module.clone();
                            let semaphore = semaphore.clone();
                            let permit = semaphore.acquire_owned().await.unwrap();
                            async move {
                                let mut out = Vec::with_capacity(config.consensus.chunk_size as usize);
                                compute_entropy_chunk(
                                    mining_address,
                                    i as u64,
                                    partition_hash.0,
                                    config.consensus.entropy_packing_iterations,
                                    config.consensus.chunk_size as usize,
                                    &mut out,
                                    self.config.chain_id
                                );

                                debug!(target: "irys::packing::progress", "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {} iterations {}", &i, &storage_module_id, &partition_hash, &mining_address, &config.consensus.entropy_packing_iterations);

                                // write the chunk
                                storage_module.write_chunk(PartitionChunkOffset::from(i), out, ChunkType::Entropy);
                                drop(permit); // drop after chunk write so the SM can apply backpressure to packing through the internal pending_writes lock write_chunk acquires
                            }
                        });

                        if i % 1000 == 0 {
                            debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {} iterations {}", chunk_range.0.start(), &i, chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
                        }
                    }
                    debug!(target: "irys::packing::done", "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {} iterations {}",  chunk_range.0.start(),chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
                }
                #[cfg(feature = "nvidia")]
                PackingType::CUDA => {
                    let chunk_size = storage_module.config.consensus.chunk_size;
                    assert_eq!(
                        chunk_size,
                        irys_types::ConsensusConfig::CHUNK_SIZE,
                        "Chunk size is not aligned with C code"
                    );

                    for chunk_range_split in split_interval(&chunk_range, self.config.max_chunks)
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

                        let semaphore = semaphore.clone();
                        // wait for the permit before spawning the thread
                        let permit = semaphore.acquire_owned().await.unwrap();
                        self.task_executor.spawn_blocking({
                            let storage_module = storage_module.clone();
                            async move {
                                let mut out: Vec<u8> = Vec::with_capacity(
                                    (num_chunks * chunk_size as u32).try_into().unwrap(),
                                );

                                capacity_pack_range_cuda_c(
                                    num_chunks,
                                    mining_address,
                                    start as u64,
                                    partition_hash,
                                    Some(
                                        storage_module.config.consensus.entropy_packing_iterations,
                                    ),
                                    &mut out,
                                    storage_module.config.consensus.entropy_packing_iterations,
                                    self.config.chain_id,
                                );
                                for i in 0..num_chunks {
                                    storage_module.write_chunk(
                                        (start + i).into(),
                                        out[(i * chunk_size as u32) as usize
                                            ..((i + 1) * chunk_size as u32) as usize]
                                            .to_vec(),
                                        ChunkType::Entropy,
                                    );
                                    if i % short_writes_before_sync == 0 {
                                        debug!("triggering sync");
                                        let _ = storage_module.sync_pending_chunks();
                                    }
                                }
                                drop(permit); // drop after chunk write so the SM can apply backpressure to packing
                            }
                        });
                        debug!(
                            target: "irys::packing::update",
                            ?start, ?end, ?storage_module_id, ?partition_hash, ?mining_address, ?storage_module.config.consensus.entropy_packing_iterations,
                            "CUDA Packed chunks"
                        );
                    }
                }
                _ => unimplemented!(),
            }

            let _ = storage_module.sync_pending_chunks();
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
        let keys = self.pending_jobs.keys().copied().collect::<Vec<usize>>();
        for key in keys {
            self.task_executor.spawn_critical(
                "packing controller",
                Self::process_jobs(
                    self.clone(),
                    key,
                    self.pending_jobs.get(&key).unwrap().clone(),
                ),
            );
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
    semaphore: PackingSemaphore,
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
                let _permit = internals
                    .semaphore
                    .acquire_many(internals.config.concurrency.into())
                    .await
                    .unwrap();
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
    use actix::Actor as _;
    use irys_packing::capacity_single::compute_entropy_chunk;
    use irys_storage::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        partition::{PartitionAssignment, PartitionHash},
        Config, ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
        StorageSyncConfig,
    };
    use reth::tasks::TaskManager;
    use std::{sync::Arc, time::Duration};

    use crate::packing::{
        cast_vec_u8_to_vec_u8_array, wait_for_packing, PackingActor, PackingConfig, PackingRequest,
    };

    #[actix::test]
    async fn test_packing_actor() -> eyre::Result<()> {
        // setup
        let partition_hash = PartitionHash::zero();
        let num_chunks = 50;
        let to_pack = 10;
        let packing_end = num_chunks - to_pack;

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_packing_actor"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                entropy_packing_iterations: 1000000,
                num_chunks_in_partition: num_chunks,
                chunk_size: 32,
                ..ConsensusConfig::testnet()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            packing: irys_types::PackingConfig {
                cpu_packing_concurrency: 1,
                gpu_packing_batch_size: 1,
            },
            base_directory: base_path.clone(),
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);
        let packing_config = PackingConfig::new(&config);

        use irys_storage::ie;

        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                irys_types::partition_chunk_offset_ie!(0, num_chunks),
                "hdd0".into(),
            )],
        }];
        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = Arc::new(StorageModule::new(storage_module_info, &config)?);

        let request = PackingRequest {
            storage_module: storage_module.clone(),
            chunk_range: PartitionChunkRange(irys_types::partition_chunk_offset_ie!(
                0,
                packing_end
            )),
        };
        // Create an instance of the mempool actor
        let task_manager = TaskManager::current();
        let sm_ids = vec![storage_module.id];
        let packing = PackingActor::new(task_manager.executor(), sm_ids, packing_config);
        let packing_addr = packing.start();

        // action
        packing_addr.send(request).await?;
        wait_for_packing(packing_addr, Some(Duration::from_secs(99999))).await?;
        storage_module.sync_pending_chunks()?;

        // assert
        // check that the chunks are marked as packed
        let intervals = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(
            intervals,
            vec![ie(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(packing_end)
            )]
        );

        let intervals2 = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            intervals2,
            vec![ie(
                PartitionChunkOffset::from(packing_end),
                PartitionChunkOffset::from(num_chunks)
            )]
        );

        let stored_entropy = storage_module.read_chunks(ie(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(packing_end),
        ))?;
        // verify the packing
        for i in 0..packing_end {
            let chunk = stored_entropy.get(&PartitionChunkOffset::from(i)).unwrap();

            let mut out = Vec::with_capacity(config.consensus.chunk_size as usize);
            compute_entropy_chunk(
                config.node_config.miner_address(),
                i as u64,
                partition_hash.0,
                config.consensus.entropy_packing_iterations,
                config.consensus.chunk_size.try_into().unwrap(),
                &mut out,
                config.consensus.chain_id,
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
        let _c2 = cast_vec_u8_to_vec_u8_array::<3>(v);
    }
}
