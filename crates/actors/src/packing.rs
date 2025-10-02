use eyre::eyre;
use futures::StreamExt as _;
use irys_domain::{ChunkType, StorageModule};
use irys_packing::{capacity_single::compute_entropy_chunk, PackingType, PACKING_TYPE};
use irys_types::{
    ii, partition_chunk_offset_ii, remote_packing::RemotePackingRequest, Config,
    PartitionChunkOffset, PartitionChunkRange, RemotePackingConfig,
};
use reth::revm::primitives::bytes::{Bytes, BytesMut};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::error::{SendError, TrySendError},
        Notify, Semaphore,
    },
    task::yield_now,
};
use tracing::{debug, error, span, trace, warn, Level};

#[cfg(feature = "nvidia")]
use {irys_packing::capacity_pack_range_cuda_c, irys_types::split_interval};

#[derive(Debug, Clone)]
pub struct PackingRequest {
    pub storage_module: Arc<StorageModule>,
    pub chunk_range: PartitionChunkRange,
}

pub type AtomicPackingJobQueue = Arc<RwLock<VecDeque<PackingRequest>>>;
pub type PackingJobsBySM = HashMap<usize, AtomicPackingJobQueue>;

pub type PackingSemaphore = Arc<Semaphore>;

#[derive(Debug, Clone)]
/// Packing service state
pub struct PackingService {
    /// list of all the pending packing jobs
    pending_jobs: PackingJobsBySM,
    /// semaphore to control concurrency -- sm_id => semaphore
    semaphore: PackingSemaphore,
    /// Atomic counter of the number of active workers - used primarily to determine when packing has finished
    active_workers: Arc<AtomicUsize>,
    /// Notifier for activity/idle transitions
    notify: Arc<Notify>,
    /// packing process configuration
    packing_config: PackingConfig,
    /// Top level config
    config: Arc<Config>,
}

#[derive(Debug, Clone)]
/// configuration for the packing service
pub struct PackingConfig {
    pub poll_duration: Duration,
    /// Max. number of packing threads for CPU packing
    pub concurrency: u16,
    /// Max. number of chunks send to GPU packing
    #[cfg(feature = "nvidia")]
    pub max_chunks: u32,
    /// Irys chain id
    pub chain_id: u64,
    /// Configuration for remote packing hosts
    pub remotes: Vec<RemotePackingConfig>,
}

impl PackingConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            poll_duration: Duration::from_millis(1000),
            concurrency: config.node_config.packing.local.cpu_packing_concurrency,
            chain_id: config.consensus.chain_id,
            #[cfg(feature = "nvidia")]
            max_chunks: config.node_config.packing.local.gpu_packing_batch_size,
            remotes: config.node_config.packing.remote.clone(),
        }
    }
}

// log every 1000 chunks
const LOG_PER_CHUNKS: u32 = 1000;

impl PackingService {
    /// creates a new packing service
    pub fn new(storage_module_ids: Vec<usize>, config: Arc<Config>) -> Self {
        let packing_config = PackingConfig::new(&config);
        let semaphore = Arc::new(Semaphore::new(packing_config.concurrency.into()));
        let pending_jobs = storage_module_ids
            .iter()
            .map(|s| (*s, Arc::new(RwLock::new(VecDeque::with_capacity(32)))))
            .collect();
        Self {
            pending_jobs,
            semaphore,
            packing_config,
            config,
            active_workers: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    async fn process_jobs(
        self,
        pending_jobs: AtomicPackingJobQueue,
        runtime_handle: tokio::runtime::Handle,
    ) {
        let mut was_active = false;
        'main: loop {
            if was_active {
                // basic atomic counter so we can keep track of the number of active workers
                self.active_workers.fetch_sub(1, Ordering::Relaxed);
                // signal potential idle transition
                self.notify.notify_waiters();
                was_active = false
            }
            // block as the compiler can't reason about explicit read guard drops with Send bounds apparently
            let front = {
                let mut pending_write_guard = pending_jobs
                    .write()
                    .expect("Unable to acquire pending jobs write lock");
                pending_write_guard.pop_front()
            };

            let next_range = match front {
                Some(v) => v,
                None => {
                    // debug!("no packing requests in queue, sleeping...");
                    tokio::time::sleep(self.packing_config.poll_duration).await;
                    continue;
                }
            };

            self.active_workers.fetch_add(1, Ordering::Relaxed);
            // signal activity observed
            self.notify.notify_waiters();
            was_active = true;

            // TODO: should we be validating if the requested range is valid?
            let PackingRequest {
                storage_module,
                chunk_range: job_chunk_range,
            } = next_range;

            let assignment = match storage_module.partition_assignment() {
                Some(v) => v,
                None => {
                    warn!(target:"irys::packing", "Partition assignment for storage module {} is `None`, cannot pack requested range {:?}", &storage_module.id, &job_chunk_range);
                    continue;
                }
            };

            let mining_address = assignment.miner_address;
            let partition_hash = assignment.partition_hash;
            let storage_module_id = storage_module.id;
            let semaphore = self.semaphore.clone();
            let chunk_size = self.config.consensus.chunk_size as usize;

            let mut range_start = *job_chunk_range.0.start();
            let range_end = *job_chunk_range.0.end();

            let short_writes_before_sync: u32 = (self
                .config
                .node_config
                .storage
                .num_writes_before_sync
                .div_ceil(2))
            .try_into()
            .expect("Should be able to convert min_writes_before_sync to u32");

            // TODO: double-check that the requested job is still valid (i.e check the range hasn't been packed already - if it has, fragment it as required into smaller jobs and resubmit them)

            // try to use a remote packing service
            'outer: for remote in &self.packing_config.remotes {
                // recompute this so we account for partial completions from other packing remotes
                let current_chunk_range =
                    PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

                let RemotePackingConfig { url, timeout } = remote;

                let v1_url = format!("{}/v1", &url);
                // try to connect

                let _span = span!(
                    Level::DEBUG,
                    "remote_packing",
                    url = v1_url,
                    target = "irys::packing::remote"
                );

                // let mut headers = HeaderMap::new();
                // // TODO: actual auth, don't just send the secret in the headers (at least hash it with a nonce)
                // headers.append(
                //     "X-Irys-Packing-Auth-Secret",
                //     HeaderValue::from_str(secret)
                //         .expect("packing secret must be a valid header value"),
                // );
                // let mut client = reqwest::Client::builder().default_headers(headers);

                let mut client = reqwest::Client::builder();

                if let Some(timeout) = timeout {
                    client = client.connect_timeout(*timeout).read_timeout(*timeout);
                };

                let client = client
                    .build()
                    .expect("Building the reqwest client should not fail");

                debug!("Attempting to connect to remote packing host {}", &v1_url);
                if let Err(e) = client.get(format!("{}/info", &v1_url)).send().await {
                    // skip this client
                    // TODO: add some exponential backoff/global removal
                    warn!("Unable to connect to remote packing host {} - {}", url, e);
                    continue;
                };

                // now produce the packing request
                let request = RemotePackingRequest {
                    mining_address,
                    partition_hash,
                    chunk_range: current_chunk_range,
                    chain_id: self.packing_config.chain_id,
                    chunk_size: chunk_size as u64,
                    entropy_packing_iterations: self.config.consensus.entropy_packing_iterations,
                };

                let response = match client
                    .post(format!("{}/pack", &v1_url))
                    .json(&request)
                    .send()
                    .await
                {
                    Ok(res) => res,
                    Err(e) => {
                        error!("Error sending packing request to {} - {}", &v1_url, &e);
                        continue;
                    }
                };

                let mut stream = response.bytes_stream();

                let mut buffer = BytesMut::with_capacity(
                    (self.config.consensus.chunk_size * 2).try_into().unwrap(),
                );

                let mut process_chunk = |chunk_bytes: Bytes| {
                    // TODO: move our use of Vec for bytes to, well, Bytes, so we get much cheaper copies etc

                    storage_module.write_chunk(
                        irys_types::PartitionChunkOffset(range_start),
                        chunk_bytes.to_vec(),
                        ChunkType::Entropy,
                    );

                    if range_start % LOG_PER_CHUNKS == 0 {
                        debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {} iterations {}", current_chunk_range.0.start(), &range_start, current_chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
                    }
                    if range_start % short_writes_before_sync == 0 {
                        debug!("triggering sync");
                        // TODO: we shouldn't ignore these errors - if we get one, we should try to sample the SM's state and re-compute job(s) (depending on fragmentation) to pack the range this job would've covered
                        let _ = storage_module.sync_pending_chunks();
                        // don't need this as stream.next() uses await, so it naturally yields
                        // yield_now().await // so the shutdown can stop us
                    }

                    range_start += 1;
                };

                while let Some(chunk_result) = stream.next().await {
                    let chunk = match chunk_result {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Error getting chunk {:?}", &e);
                            // we assume this means the packing remote has failed
                            // so we move to the next one
                            continue 'outer;
                        }
                    };

                    buffer.extend_from_slice(&chunk);

                    // Process complete chunks
                    while buffer.len() >= chunk_size {
                        let chunk_to_process = buffer.split_to(chunk_size).freeze();
                        process_chunk(chunk_to_process);
                    }
                }

                // Process any remaining bytes
                if !buffer.is_empty() {
                    process_chunk(buffer.freeze());
                }

                // we finished (>= because when we process the last chunk we still += 1)
                if range_start >= range_end {
                    continue 'main;
                }
            }

            // we fall back to local packing if the remote packing systems fail
            // TODO: maybe allow for no local packing? if so, push the Jobs CURRENT state back to the pending queue

            // recompute this so we account for partial completions from other packing remotes
            let current_chunk_range =
                PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

            match PACKING_TYPE {
                PackingType::CPU => {
                    for i in range_start..=range_end {
                        // each semaphore permit corresponds to a single chunk to be packed, as we assume it'll use an entire CPU thread's worth of compute.
                        // when we implement GPU packing, this is where we need to fork the logic between the two methods - GPU can take larger contiguous segments
                        // whereas CPU will do this permit system

                        // TODO: have stateful executor threads / an arena for entropy chunks so we don't have to allocate chunks all over the place when we can just re-use
                        // TODO: improve this! use wakers instead of polling, allow for work-stealing, use a dedicated thread pool w/ lower priorities etc
                        if i % short_writes_before_sync == 0 {
                            debug!("triggering sync");
                            let _ = storage_module.sync_pending_chunks();
                            yield_now().await // so the shutdown can stop us
                        }

                        // wait for the permit before spawning the thread
                        let permit = semaphore
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("Failure acquiring a CPU packing semaphore");

                        let config = self.config.clone();
                        let storage_module_clone = storage_module.clone();
                        let chain_id = self.packing_config.chain_id;
                        runtime_handle.clone().spawn_blocking(move || {
                            let mut out = Vec::with_capacity(config.consensus.chunk_size as usize);
                            compute_entropy_chunk(
                                mining_address,
                                i as u64,
                                partition_hash.0,
                                config.consensus.entropy_packing_iterations,
                                config.consensus.chunk_size as usize,
                                &mut out,
                                chain_id
                            );

                            debug!(target: "irys::packing::progress", "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {} iterations {}", &i, &storage_module_id, &partition_hash, &mining_address, &config.consensus.entropy_packing_iterations);

                            // write the chunk
                            storage_module_clone.write_chunk(PartitionChunkOffset::from(i), out, ChunkType::Entropy);
                            drop(permit); // drop after chunk write so the SM can apply backpressure to packing through the internal pending_writes lock write_chunk acquires
                        });

                        if i % LOG_PER_CHUNKS == 0 {
                            debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {} iterations {}", current_chunk_range.0.start(), &i, current_chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
                        }
                    }
                    trace!(target: "irys::packing::done", "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {} iterations {}", job_chunk_range.0.start(), job_chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
                }
                #[cfg(feature = "nvidia")]
                PackingType::CUDA => {
                    let chunk_size = storage_module.config.consensus.chunk_size;
                    assert_eq!(
                        chunk_size,
                        irys_types::ConsensusConfig::CHUNK_SIZE,
                        "Chunk size is not aligned with C code"
                    );

                    for chunk_range_split in
                        split_interval(&current_chunk_range, self.packing_config.max_chunks)
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
                        let permit = semaphore.clone().acquire_owned().await.unwrap();
                        let storage_module_clone = storage_module.clone();
                        let chain_id = self.packing_config.chain_id;
                        let entropy_iterations =
                            storage_module.config.consensus.entropy_packing_iterations;
                        let runtime_handle_clone = runtime_handle.clone();
                        runtime_handle.clone().spawn(async move {
                            // Run the GPU packing in a blocking task
                            let out = runtime_handle_clone
                                .spawn_blocking(move || {
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
                                        &mut out,
                                    );
                                    out
                                })
                                .await
                                .unwrap();

                            for i in 0..num_chunks {
                                storage_module_clone.write_chunk(
                                    (start + i).into(),
                                    out[(i * chunk_size as u32) as usize
                                        ..((i + 1) * chunk_size as u32) as usize]
                                        .to_vec(),
                                    ChunkType::Entropy,
                                );
                                if i % short_writes_before_sync == 0 {
                                    debug!("triggering sync");
                                    yield_now().await; // so the shutdown can stop us
                                    let _ = storage_module_clone.sync_pending_chunks();
                                }
                            }
                            drop(permit); // drop after chunk write so the SM can apply backpressure to packing
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

impl PackingService {
    /// Spawn packing controllers and return TokioServiceHandles for them
    pub fn spawn_packing_controllers(
        &self,
        runtime_handle: tokio::runtime::Handle,
    ) -> Vec<irys_types::TokioServiceHandle> {
        let mut handles = Vec::new();
        let keys = self.pending_jobs.keys().copied().collect::<Vec<usize>>();

        for key in keys {
            let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
            let self_clone = self.clone();
            let pending_jobs = self.pending_jobs.get(&key).unwrap().clone();

            let runtime_handle_clone = runtime_handle.clone();
            let handle = runtime_handle.spawn(async move {
                tokio::select! {
                    _ = Self::process_jobs(self_clone, pending_jobs, runtime_handle_clone) => {},
                    _ = shutdown_rx => {
                        tracing::info!("Packing controller for SM {} received shutdown signal", key);
                    }
                }
            });

            handles.push(irys_types::TokioServiceHandle {
                name: format!("packing_controller_sm_{}", key),
                handle,
                shutdown_signal: shutdown_tx,
            });
        }

        handles
    }
}

#[derive(Debug, Clone)]
pub struct Internals {
    pub pending_jobs: PackingJobsBySM,
    pub semaphore: PackingSemaphore,
    pub active_workers: Arc<AtomicUsize>,
    pub config: PackingConfig,
}

/// waits for any pending & active packing tasks to complete
/// A lightweight, Tokio-native handle for enqueueing packing requests and introspecting service state.
#[derive(Debug, Clone)]
pub struct PackingHandle {
    sender: tokio::sync::mpsc::Sender<PackingRequest>,
    internals: Internals,
    notify: Arc<Notify>,
}

impl PackingHandle {
    /// Enqueue a packing request with a short bounded wait if the channel is full.
    /// If the channel remains saturated past a small timeout, log a warning and drop the request.
    /// Returns Err only if the channel is closed.
    pub fn send(&self, req: PackingRequest) -> Result<(), SendError<PackingRequest>> {
        match self.sender.try_send(req) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(mut req)) => {
                let start = std::time::Instant::now();
                let max_wait = Duration::from_millis(50);
                loop {
                    if start.elapsed() >= max_wait {
                        tracing::warn!(
                            target: "irys::packing",
                            "Dropping packing request due to saturated channel after {:?}",
                            max_wait
                        );
                        // Drop to preserve responsiveness and bounded memory, mirroring legacy do_send behavior
                        return Ok(());
                    }
                    // Briefly sleep to yield CPU and give receivers a chance to make progress
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    match self.sender.try_send(req) {
                        Ok(()) => return Ok(()),
                        Err(TrySendError::Full(r)) => {
                            req = r;
                            continue;
                        }
                        Err(TrySendError::Closed(r)) => {
                            return Err(SendError(r));
                        }
                    }
                }
            }
            Err(TrySendError::Closed(req)) => Err(SendError(req)),
        }
    }

    /// Access a clone of the internals snapshot used by wait_for_packing.
    pub fn internals(&self) -> Internals {
        self.internals.clone()
    }

    /// Construct a PackingHandle from its parts.
    /// Useful for tests or custom wiring where the receiver loop is managed externally.
    pub fn from_parts(
        sender: tokio::sync::mpsc::Sender<PackingRequest>,
        internals: Internals,
    ) -> Self {
        Self {
            sender,
            internals,
            notify: Arc::new(Notify::new()),
        }
    }
}

impl PackingService {
    /// Spawn a Tokio task that receives PackingRequest messages and pushes them into the internal queues.
    /// Returns a `PackingHandle` for interacting with the service.
    pub fn spawn_tokio_service(&self, runtime_handle: tokio::runtime::Handle) -> PackingHandle {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<PackingRequest>(5_000);

        // Clone references to the shared internals so both the service and observers see the same state
        let job_queues = self.pending_jobs.clone();

        // Build an Internals snapshot that shares the same underlying Arcs as the service
        let internals = Internals {
            pending_jobs: self.pending_jobs.clone(),
            semaphore: self.semaphore.clone(),
            config: self.packing_config.clone(),
            active_workers: self.active_workers.clone(),
        };

        // Spawn the receiver loop
        let notify = self.notify.clone();
        runtime_handle.spawn(async move {
            while let Some(msg) = rx.recv().await {
                // Enqueue into the SM-specific pending queue
                if let Some(queue) = job_queues.get(&msg.storage_module.id) {
                    // Ignore poisoned lock errors by propagating panic (consistent with existing code)
                    queue.as_ref().write().unwrap().push_back(msg);
                    // signal new activity for waiters
                    notify.notify_waiters();
                } else {
                    // Unknown storage module id; this indicates a wiring/configuration bug. Drop the message but surface loudly.
                    tracing::error!(
                        target: "irys::packing",
                        sm_id = msg.storage_module.id,
                        "Received packing request for unknown storage module id; dropping request"
                    );
                    debug_assert!(
                        false,
                        "Packing request targeted unknown storage module id {}. This indicates a wiring/configuration bug.",
                        msg.storage_module.id
                    );
                }
            }
        });

        PackingHandle {
            sender: tx,
            internals,
            notify: self.notify.clone(),
        }
    }
}

/// waits for any pending & active packing tasks to complete
pub async fn wait_for_packing(
    handle: PackingHandle,
    timeout: Option<Duration>,
) -> eyre::Result<()> {
    let internals = handle.internals();
    let notify = handle.notify.clone();
    tokio::time::timeout(timeout.unwrap_or(Duration::from_secs(10)), async {
        // short warmup and require observing activity before we apply the strong barrier.
        // This avoids a race where we check before any work is observed and return prematurely.
        // Wait briefly for a state-change notification if no activity has been observed, avoiding immediate-return races.
        let mut observed_activity = false;

        loop {
            // Counts all jobs in all job queues
            let queued_jobs = internals
                .pending_jobs
                .values()
                .fold(0, |acc, x| acc + x.as_ref().read().unwrap().len());

            let available_permits = internals.semaphore.available_permits();
            let target_permits = internals.config.concurrency as usize;
            let active = internals.active_workers.load(Ordering::Relaxed);

            // Record that we've seen the system do something
            if !observed_activity
                && (queued_jobs > 0 || active > 0 || available_permits < target_permits)
            {
                observed_activity = true;
            }

            if queued_jobs == 0 && active == 0 {
                if !observed_activity {
                    // Wait briefly for a state change; if none, return early (nothing to wait for)
                    if tokio::time::timeout(Duration::from_millis(100), notify.notified())
                        .await
                        .is_ok()
                    {
                        continue;
                    }
                    break Some(());
                }

                // Strong barrier: wait for all permits to be available,
                // ensuring no local CPU workers are still running.
                let needed: u32 = internals.config.concurrency.into();
                let _permit = internals.semaphore.acquire_many(needed).await.unwrap();
                drop(_permit);
                break Some(());
            } else {
                // Wait for a state change rather than polling
                notify.notified().await
            }
        }
    })
    .await?
    .ok_or_else(|| eyre!("timed out waiting for packing to complete"))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_packing::capacity_single::compute_entropy_chunk;
    use irys_storage::ie;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        partition::{PartitionAssignment, PartitionHash},
        Config, ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
        StorageSyncConfig,
    };

    use crate::packing::{
        cast_vec_u8_to_vec_u8_array, wait_for_packing, PackingRequest, PackingService,
    };

    #[test_log::test(actix::test)]
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
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            packing: irys_types::PackingConfig {
                local: irys_types::LocalPackingConfig {
                    cpu_packing_concurrency: 1,
                    gpu_packing_batch_size: 1,
                },
                remote: Default::default(),
            },
            base_directory: base_path.clone(),
            ..NodeConfig::testing()
        };
        let config = Config::new(node_config);

        let infos = [StorageModuleInfo {
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
        // Create an instance of the packing service
        let sm_ids = vec![storage_module.id];
        let packing = PackingService::new(sm_ids, Arc::new(config.clone()));

        // Spawn packing controllers with runtime handle
        // In this test context, get the Tokio runtime handle
        let runtime_handle =
            tokio::runtime::Handle::try_current().expect("Should be running in tokio runtime");
        let _packing_handles = packing.spawn_packing_controllers(runtime_handle);
        let handle = packing.spawn_tokio_service(
            tokio::runtime::Handle::try_current().expect("Should be running in tokio runtime"),
        );

        // action
        handle.send(request)?;
        wait_for_packing(handle, Some(Duration::from_secs(99999))).await?;
        storage_module.force_sync_pending_chunks()?;

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
                i,
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
