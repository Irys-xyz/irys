use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::Duration,
};

use actix::{Actor, Context, Handler, Message};
use irys_packing::capacity_pack_range;
use irys_storage::{InclusiveInterval, StorageModule};
use irys_types::PartitionChunkRange;
use reth::tasks::TaskExecutor;
use tokio::{runtime::Handle, sync::Semaphore};
use tracing::{debug, warn};

#[derive(Debug, Message, Clone)]
#[rtype("()")]
struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

type PackingJobs = Arc<RwLock<VecDeque<PackingRequest>>>;

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

            for i in chunk_range.start()..chunk_range.end() {
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
                    let entropy_chunk =
                        capacity_pack_range(mining_address, i as u64, partition_hash, None)
                            .unwrap();
                    // computation has finished, release permit
                    drop(permit);
                    // write the chunk
                    storage_module.write_chunk(i, entropy_chunk, irys_storage::ChunkType::Entropy);
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
