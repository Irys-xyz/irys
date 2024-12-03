use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::Duration,
};

use actix::{Actor, Context, Handler, Message, MessageResponse};

use irys_packing::{capacity_single::compute_entropy_chunk, PACKING_SHA_1_5_S};
use irys_storage::{
    ii, initialize_storage_files, ChunkType, InclusiveInterval, StorageModule, StorageModuleInfo,
};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    partition::{PartitionAssignment, PartitionHash},
    Address, PartitionChunkRange, StorageConfig,
};
use reth::tasks::{TaskExecutor, TaskManager};
use tokio::{
    runtime::Handle,
    sync::Semaphore,
    time::{sleep, timeout},
};
use tracing::{debug, warn};

#[derive(Debug, Message, Clone)]
#[rtype("()")]
pub struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

pub type PackingJobs = Arc<RwLock<VecDeque<PackingRequest>>>;

#[derive(Debug, Clone)]
/// Packing actor state
pub struct PackingActor {
    /// used to execute the internal job poll future
    actix_runtime_handle: Handle,
    /// used to spawn threads to perform packing
    task_executor: TaskExecutor,
    /// list of all the pending packing jobs
    pending_jobs: PackingJobs,
    /// semaphore to control concurrency
    semaphore: Arc<Semaphore>,
    /// packing process configuration
    config: PackingConfig,
}

#[derive(Debug, Clone)]
/// configuration for the packing actor
pub struct PackingConfig {
    poll_duration: Duration,
    concurrency: u16,
}
impl Default for PackingConfig {
    fn default() -> Self {
        Self {
            poll_duration: Duration::from_millis(1000),
            concurrency: 4,
        }
    }
}

impl PackingActor {
    /// creates a new packing actor
    pub fn new(
        actix_runtime_handle: Handle,
        task_executor: TaskExecutor,
        config: Option<PackingConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        Self {
            actix_runtime_handle,
            task_executor,
            pending_jobs: Arc::new(RwLock::new(VecDeque::with_capacity(32))),
            semaphore: Arc::new(Semaphore::new(config.concurrency.into())),
            config,
        }
    }

    async fn process_jobs(self) {
        loop {
            // block as the compiler can't reason about explicit read guard drops with Send bounds apparently
            let front = {
                let pending_read_guard = self.pending_jobs.read().unwrap();
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
                    warn!(target:"irys::packing", "Partition assignment for storage module {} is `None`, cannot pack requested range {:?}", &storage_module.module_num, &chunk_range);
                    self.pending_jobs.write().unwrap().pop_front();
                    continue;
                }
            };

            let mining_address = assignment.miner_address;
            let partition_hash = assignment.partition_hash;
            let chunk_size = storage_module.config.chunk_size;

            for i in chunk_range.start()..=chunk_range.end() {
                // each semaphore permit corresponds to a single chunk to be packed, as we assume it'll use an entire CPU thread's worth of compute.
                // when we implement GPU packing, this is where we need to fork the logic between the two methods - GPU can take larger contiguous segments
                // whereas CPU will do this permit system

                // TODO: have stateful executor threads / an arena for entropy chunks so we don't have to allocate chunks all over the place when we can just re-use
                // TODO: improve this! use wakers instead of polling, allow for work-stealing, use a dedicated thread pool w/ lower priorities etc.
                let semaphore: Arc<Semaphore> = self.semaphore.clone();
                let storage_module = storage_module.clone();
                // wait for the permit before spawning the thread
                let permit = semaphore.acquire_owned().await.unwrap();
                self.task_executor.spawn_blocking(async move {
                    let mut out = Vec::with_capacity(chunk_size.try_into().unwrap());
                    compute_entropy_chunk(
                        mining_address,
                        i as u64,
                        partition_hash.0,
                        PACKING_SHA_1_5_S,
                        chunk_size.try_into().unwrap(),
                        &mut out,
                    );
                    // computation is done, release semaphore
                    drop(permit);
                    // write the chunk
                    debug!(target: "irys::packing", "Writing chunk {} to {}", &i, &storage_module.module_num);
                    storage_module.write_chunk(i, out, ChunkType::Entropy);
                });
            }

            // Remove from queue once complete
            let _ = self.pending_jobs.write().unwrap().pop_front();
        }
    }
}

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
        self.actix_runtime_handle
            .spawn(Self::process_jobs(self.clone()));

        Context::new().run(self)
    }

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(5_000);
    }
}

impl Handler<PackingRequest> for PackingActor {
    type Result = ();

    fn handle(&mut self, msg: PackingRequest, ctx: &mut Self::Context) -> Self::Result {
        self.pending_jobs.write().unwrap().push_back(msg);
    }
}

#[derive(Debug, Message, Clone)]
#[rtype("Internals")]
pub struct GetInternals();

#[derive(Debug, MessageResponse, Clone)]

pub struct Internals {
    pending_jobs: PackingJobs,
    semaphore: Arc<Semaphore>,
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

#[actix::test]
async fn test_packing_actor() -> eyre::Result<()> {
    let infos = vec![StorageModuleInfo {
        module_num: 0,
        partition_assignment: Some(PartitionAssignment {
            partition_hash: PartitionHash::zero(),
            miner_address: Address::random(),
            ledger_num: None,
            slot_index: None,
        }),
        submodules: vec![
            (ii(0, 4), "hdd0-4TB".to_string()), // 0 to 4 inclusive
        ],
    }];

    let tmp_dir = setup_tracing_and_temp_dir(Some("test_packing_actor"), false);
    let base_path = tmp_dir.path().to_path_buf();
    initialize_storage_files(&base_path, &infos)?;

    // Override the default StorageModule config for testing
    let config = StorageConfig {
        min_writes_before_sync: 1,
        ..Default::default()
    };

    // Create a StorageModule with the specified submodules and config
    let storage_module_info = &infos[0];
    let storage_module = Arc::new(StorageModule::new(
        &base_path,
        storage_module_info,
        Some(config),
    ));
    let request = PackingRequest {
        storage_module: storage_module.clone(),
        chunk_range: ii(0, 3).into(),
    };
    // Create an instance of the mempool actor
    let task_manager = TaskManager::current();

    let packing = PackingActor::new(Handle::current(), task_manager.executor(), None);

    let packing_addr = packing.start();

    packing_addr.send(request).await?;

    let internals = packing_addr.send(GetInternals()).await?;
    // busypoll the job queue

    timeout(Duration::from_secs(5), async {
        loop {
            if internals.pending_jobs.read().unwrap().len() == 0 {
                // try to get all the semaphores - this is how we know that the packing is done
                let _permit = internals
                    .semaphore
                    .acquire_many(internals.config.concurrency as u32)
                    .await
                    .unwrap();

                break Some(());
            } else {
                sleep(Duration::from_millis(100)).await
            }
        }
    })
    .await?
    .unwrap();

    storage_module.sync_pending_chunks()?;
    // check that the chunks are marked as packed
    let intervals = storage_module.get_intervals(ChunkType::Entropy);

    assert_eq!(intervals, vec![ii(0, 3)]);

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
