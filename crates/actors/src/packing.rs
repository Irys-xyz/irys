use eyre::eyre;
use futures::StreamExt as _;
use irys_domain::{ChunkType, StorageModule};
use irys_packing::{capacity_single::compute_entropy_chunk, PackingType, PACKING_TYPE};
use irys_types::{
    ii, partition::PartitionHash, partition_chunk_offset_ii, remote_packing::RemotePackingRequest,
    Config, PartitionChunkOffset, PartitionChunkRange, RemotePackingConfig,
};
use reth::revm::primitives::bytes::{Bytes, BytesMut};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::error::{SendError, TrySendError},
        oneshot, Notify, Semaphore,
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
pub type PackingQueues = Arc<RwLock<PackingJobsBySM>>;

pub type PackingSemaphore = Arc<Semaphore>;

#[derive(Debug, Clone)]
/// Packing service state
pub struct PackingService {
    /// list of all the pending packing jobs
    pending_jobs: PackingQueues,
    /// Semaphore limiting concurrent packing operations across all storage modules
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

const LOG_PER_CHUNKS: u32 = 1000;

impl PackingService {
    pub fn new(config: Arc<Config>) -> Self {
        let packing_config = PackingConfig::new(&config);
        let semaphore = Arc::new(Semaphore::new(packing_config.concurrency.into()));
        let pending_jobs: PackingQueues = Arc::new(RwLock::new(HashMap::new()));
        Self {
            pending_jobs,
            semaphore,
            packing_config,
            config,
            active_workers: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Try to poll a job from any non-empty queue, or wait for new work
    async fn poll_next_job(&self) -> Option<PackingRequest> {
        // NOTE: Must capture Notified future BEFORE checking queue to avoid race:
        // If notification arrives between queue check and notified() call, we'd deadlock
        let notified = self.notify.notified();
        let job = {
            let map_guard = self
                .pending_jobs
                .read()
                .expect("Unable to acquire pending jobs map read lock");
            let mut popped = None;
            for queue in map_guard.values() {
                let mut q = queue
                    .as_ref()
                    .write()
                    .expect("Unable to acquire pending queue write lock");
                if let Some(req) = q.pop_front() {
                    popped = Some(req);
                    break;
                }
            }
            popped
        };

        match job {
            Some(req) => Some(req),
            None => {
                tokio::select! {
                    _ = notified => {},
                    _ = tokio::time::sleep(self.packing_config.poll_duration) => {},
                }
                None
            }
        }
    }

    /// Process a stream of packed chunks from a remote packing service
    async fn process_remote_chunk_stream(
        &self,
        mut stream: impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
        storage_module: Arc<StorageModule>,
        current_chunk_range: &PartitionChunkRange,
        mut range_start: u32,
        _range_end: u32,
        short_writes_before_sync: u32,
        storage_module_id: usize,
        partition_hash: PartitionHash,
        mining_address: [u8; 20],
    ) -> Result<u32, String> {
        let chunk_size = self.config.consensus.chunk_size as usize;
        // Buffer capacity is 2x chunk_size to handle worst-case fragmentation across network packets
        let mut buffer =
            BytesMut::with_capacity((self.config.consensus.chunk_size * 2).try_into().unwrap());
        let notify = self.notify.clone();

        let mut process_chunk = |chunk_bytes: Bytes| {
            storage_module.write_chunk(
                irys_types::PartitionChunkOffset(range_start),
                chunk_bytes.to_vec(),
                ChunkType::Entropy,
            );
            notify.notify_waiters();

            if range_start % LOG_PER_CHUNKS == 0 {
                debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {:?} iterations {}", current_chunk_range.0.start(), &range_start, current_chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
            }
            if range_start % short_writes_before_sync == 0 {
                debug!("triggering sync");
                let _ = storage_module.sync_pending_chunks();
                notify.notify_waiters();
            }

            range_start += 1;
        };

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| format!("Error getting chunk: {:?}", e))?;
            buffer.extend_from_slice(&chunk);

            while buffer.len() >= chunk_size {
                let chunk_to_process = buffer.split_to(chunk_size).freeze();
                process_chunk(chunk_to_process);
            }
        }

        if !buffer.is_empty() {
            process_chunk(buffer.freeze());
        }

        Ok(range_start)
    }

    /// Attempt to pack using remote packing services
    /// Returns Ok(final_range_start) if successful, Err if all remotes fail
    async fn try_remote_packing(
        &self,
        storage_module: Arc<StorageModule>,
        mut range_start: u32,
        range_end: u32,
        mining_address: [u8; 20],
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<u32, ()> {
        for remote in &self.packing_config.remotes {
            let current_chunk_range =
                PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

            let RemotePackingConfig { url, timeout } = remote;
            let v1_url = format!("{}/v1", &url);

            let _span = span!(
                Level::DEBUG,
                "remote_packing",
                url = v1_url,
                target = "irys::packing::remote"
            );

            let mut client = reqwest::Client::builder();
            if let Some(timeout) = timeout {
                client = client.connect_timeout(*timeout).read_timeout(*timeout);
            }

            let client = client
                .build()
                .expect("Building the reqwest client should not fail");

            debug!("Attempting to connect to remote packing host {}", &v1_url);
            if let Err(e) = client.get(format!("{}/info", &v1_url)).send().await {
                warn!("Unable to connect to remote packing host {} - {}", url, e);
                continue;
            }

            let request = RemotePackingRequest {
                mining_address: mining_address.into(),
                partition_hash,
                chunk_range: current_chunk_range,
                chain_id: self.packing_config.chain_id,
                chunk_size: self.config.consensus.chunk_size,
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

            let stream = response.bytes_stream();

            match self
                .process_remote_chunk_stream(
                    stream,
                    storage_module.clone(),
                    &current_chunk_range,
                    range_start,
                    range_end,
                    short_writes_before_sync,
                    storage_module_id,
                    partition_hash,
                    mining_address,
                )
                .await
            {
                Ok(new_range_start) => {
                    range_start = new_range_start;
                    if range_start >= range_end {
                        return Ok(range_start);
                    }
                }
                Err(e) => {
                    error!("Error processing remote chunk stream: {}", e);
                    continue;
                }
            }
        }

        Err(())
    }

    /// Pack chunks using CPU
    async fn pack_with_cpu(
        &self,
        runtime_handle: tokio::runtime::Handle,
        storage_module: Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: [u8; 20],
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) {
        let range_start = *chunk_range.0.start();
        let range_end = *chunk_range.0.end();
        let semaphore = self.semaphore.clone();

        for i in range_start..=range_end {
            if i % short_writes_before_sync == 0 {
                let _ = storage_module.sync_pending_chunks();
                yield_now().await
            }

            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Failure acquiring a CPU packing semaphore");

            let config = self.config.clone();
            let storage_module_clone = storage_module.clone();
            let chain_id = self.packing_config.chain_id;
            let notify = self.notify.clone();
            runtime_handle.clone().spawn_blocking(move || {
                let mut out = Vec::with_capacity(config.consensus.chunk_size as usize);
                compute_entropy_chunk(
                    mining_address.into(),
                    i as u64,
                    partition_hash.0,
                    config.consensus.entropy_packing_iterations,
                    config.consensus.chunk_size as usize,
                    &mut out,
                    chain_id,
                );

                debug!(target: "irys::packing::progress", "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {:?} iterations {}", &i, &storage_module_id, &partition_hash, &mining_address, &config.consensus.entropy_packing_iterations);

                storage_module_clone.write_chunk(PartitionChunkOffset::from(i), out, ChunkType::Entropy);
                drop(permit);
                notify.notify_waiters();
            });

            if i % LOG_PER_CHUNKS == 0 {
                debug!(target: "irys::packing::update", "CPU Packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {:?} iterations {}", chunk_range.0.start(), &i, chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
            }
        }
        trace!(target: "irys::packing::done", "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {:?} iterations {}", chunk_range.0.start(), chunk_range.0.end(), &storage_module_id, &partition_hash, &mining_address, &storage_module.config.consensus.entropy_packing_iterations);
    }

    /// Pack chunks using CUDA/GPU
    #[cfg(feature = "nvidia")]
    async fn pack_with_cuda(
        &self,
        runtime_handle: tokio::runtime::Handle,
        storage_module: Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: [u8; 20],
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) {
        let chunk_size = storage_module.config.consensus.chunk_size;
        assert_eq!(
            chunk_size,
            irys_types::ConsensusConfig::CHUNK_SIZE,
            "Chunk size is not aligned with C code"
        );

        let semaphore = self.semaphore.clone();

        for chunk_range_split in split_interval(&chunk_range, self.packing_config.max_chunks)
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

            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("Failure acquiring a CUDA packing semaphore");
            let storage_module_clone = storage_module.clone();
            let chain_id = self.packing_config.chain_id;
            let entropy_iterations = storage_module.config.consensus.entropy_packing_iterations;
            let runtime_handle_clone = runtime_handle.clone();

            runtime_handle.clone().spawn(async move {
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
                        yield_now().await;
                        let _ = storage_module_clone.sync_pending_chunks();
                    }
                }
                drop(permit);
            });

            debug!(
                target: "irys::packing::update",
                ?start, ?end, ?storage_module_id, ?partition_hash, ?mining_address, ?storage_module.config.consensus.entropy_packing_iterations,
                "CUDA Packed chunks"
            );
        }
    }

    async fn process_jobs(self, runtime_handle: tokio::runtime::Handle) {
        let mut was_active = false;
        'main: loop {
            if was_active {
                self.active_workers.fetch_sub(1, Ordering::Relaxed);
                self.notify.notify_waiters();
                was_active = false
            }

            let next_range = match self.poll_next_job().await {
                Some(job) => job,
                None => continue,
            };

            self.active_workers.fetch_add(1, Ordering::Relaxed);
            self.notify.notify_waiters();
            was_active = true;

            // TODO: Validate requested range isn't already packed to avoid redundant work.
            //       If packed, fragment into unpacked sub-ranges and re-enqueue.
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

            let range_start = *job_chunk_range.0.start();
            let range_end = *job_chunk_range.0.end();

            // Use half the normal sync frequency for packing operations to reduce disk I/O overhead
            // while maintaining reasonable durability guarantees
            let short_writes_before_sync: u32 = (self
                .config
                .node_config
                .storage
                .num_writes_before_sync
                .div_ceil(2))
            .try_into()
            .expect("Should be able to convert min_writes_before_sync to u32");

            if self
                .try_remote_packing(
                    storage_module.clone(),
                    range_start,
                    range_end,
                    **mining_address,
                    partition_hash,
                    storage_module_id,
                    short_writes_before_sync,
                )
                .await
                .is_ok()
            {
                continue 'main;
            }

            let current_chunk_range =
                PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

            match PACKING_TYPE {
                PackingType::CPU => {
                    self.pack_with_cpu(
                        runtime_handle.clone(),
                        storage_module.clone(),
                        current_chunk_range,
                        **mining_address,
                        partition_hash,
                        storage_module_id,
                        short_writes_before_sync,
                    )
                    .await;
                }
                #[cfg(feature = "nvidia")]
                PackingType::CUDA => {
                    self.pack_with_cuda(
                        runtime_handle.clone(),
                        storage_module.clone(),
                        current_chunk_range,
                        **mining_address,
                        partition_hash,
                        storage_module_id,
                        short_writes_before_sync,
                    )
                    .await;
                }
                _ => unimplemented!(),
            }

            let _ = storage_module.sync_pending_chunks();
        }
    }
}

/// Transfers ownership of a byte vector to an array-of-arrays vector without copying.
///
/// # Safety
///
/// This function performs a zero-copy transformation by reinterpreting the memory layout.
/// The input Vec's allocation is transferred to the output Vec, preventing double-free.
///
/// # Panics
///
/// Panics if `input.len()` is not evenly divisible by `N`.
#[cfg(test)]
#[inline]
fn cast_vec_u8_to_vec_u8_array<const N: usize>(input: Vec<u8>) -> Vec<[u8; N]> {
    assert!(input.len() % N == 0, "wrong input N {}", N);
    let length = input.len() / N;
    let ptr = input.as_ptr() as *const [u8; N];
    std::mem::forget(input);

    // SAFETY: We've verified input.len() % N == 0, ensuring alignment is valid.
    // mem::forget prevents double-free as ownership transfers to the returned Vec.
    unsafe { Vec::from_raw_parts(ptr as *mut [u8; N], length, length) }
}

impl PackingService {
    /// Spawn multiple packing controllers (based on configured concurrency) that scan all queues.
    /// Returns TokioServiceHandles for each controller.
    pub fn spawn_packing_controllers(
        &self,
        runtime_handle: tokio::runtime::Handle,
    ) -> Vec<irys_types::TokioServiceHandle> {
        let mut handles = Vec::new();
        let controller_count = std::cmp::max(1_usize, self.packing_config.concurrency as usize);

        for i in 0..controller_count {
            let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
            let self_clone = self.clone();
            let runtime_handle_clone = runtime_handle.clone();

            let handle = runtime_handle.spawn(async move {
                tokio::select! {
                    _ = Self::process_jobs(self_clone, runtime_handle_clone) => {},
                    _ = shutdown_rx => {
                        tracing::info!("Packing controller {} received shutdown signal", i);
                    }
                }
            });

            handles.push(irys_types::TokioServiceHandle {
                name: format!("packing_controller_{}", i),
                handle,
                shutdown_signal: shutdown_tx,
            });
        }

        handles
    }
}

#[derive(Debug, Clone)]
pub struct Internals {
    pub pending_jobs: PackingQueues,
    pub semaphore: PackingSemaphore,
    pub active_workers: Arc<AtomicUsize>,
    pub config: PackingConfig,
}

#[derive(Debug)]
pub enum PackingServiceMessage {
    Drain { respond_to: oneshot::Sender<()> },
}

#[derive(Debug, Clone)]
pub struct PackingIdleWaiter {
    packing_service_sender: tokio::sync::mpsc::Sender<PackingServiceMessage>,
}

impl PackingIdleWaiter {
    pub async fn wait_for_idle(&self, timeout: Option<Duration>) -> eyre::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.packing_service_sender
            .send(PackingServiceMessage::Drain { respond_to: tx })
            .await
            .map_err(|e| eyre!("control channel closed: {}", e))?;
        tokio::time::timeout(timeout.unwrap_or(Duration::from_secs(10)), async move {
            rx.await
                .map_err(|e| eyre!("waiter dropped: {}", e))
                .map(|_| ())
        })
        .await
        .map_err(|_| eyre!("timed out waiting for packing to become idle"))??;
        Ok(())
    }
}

/// waits for any pending & active packing tasks to complete
/// A lightweight, Tokio-native handle for enqueueing packing requests and introspecting service state.
#[derive(Debug, Clone)]
pub struct PackingHandle {
    sender: tokio::sync::mpsc::Sender<PackingRequest>,
    internals: Internals,

    packing_service_sender: tokio::sync::mpsc::Sender<PackingServiceMessage>,
}

impl PackingHandle {
    /// Enqueue a packing request. If the bounded channel is full, drop immediately with a warning.
    /// Returns Err only if the channel is closed.
    pub fn send(&self, req: PackingRequest) -> Result<(), SendError<PackingRequest>> {
        match self.sender.try_send(req) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                tracing::warn!(target: "irys::packing", "Dropping packing request due to saturated channel");
                Ok(())
            }
            Err(TrySendError::Closed(req)) => Err(SendError(req)),
        }
    }

    /// Access a clone of the internals snapshot used by wait_for_packing.
    pub fn internals(&self) -> Internals {
        self.internals.clone()
    }

    /// Access a clone of the underlying sender for enqueueing packing requests.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<PackingRequest> {
        self.sender.clone()
    }

    /// Create a PackingIdleWaiter that can wait for the service to become idle.
    pub fn waiter(&self) -> PackingIdleWaiter {
        PackingIdleWaiter {
            packing_service_sender: self.packing_service_sender.clone(),
        }
    }

    /// Construct a PackingHandle from its parts.
    /// Useful for tests or custom wiring where the receiver loop is managed externally.
    pub fn from_parts(
        sender: tokio::sync::mpsc::Sender<PackingRequest>,
        internals: Internals,
    ) -> Self {
        let (packing_service_sender, _ctrl_rx) =
            tokio::sync::mpsc::channel::<PackingServiceMessage>(1);
        Self {
            sender,
            internals,

            packing_service_sender,
        }
    }
}

pub type PackingSender = tokio::sync::mpsc::Sender<PackingRequest>;
pub type PackingReceiver = tokio::sync::mpsc::Receiver<PackingRequest>;

impl PackingService {
    /// Check if the packing system is idle (no queued jobs, no active workers, all permits available)
    fn is_system_idle(internals: &Internals) -> bool {
        let queued_jobs = {
            let map = internals.pending_jobs.read().unwrap();
            map.values()
                .fold(0, |acc, q| acc + q.as_ref().read().unwrap().len())
        };
        let available_permits = internals.semaphore.available_permits();
        let target_permits = internals.config.concurrency as usize;
        let active = internals.active_workers.load(Ordering::Relaxed);

        queued_jobs == 0 && active == 0 && available_permits == target_permits
    }

    /// Flush all pending waiters by sending idle notification
    fn flush_waiters(pending_waiters: &Mutex<Vec<oneshot::Sender<()>>>) {
        let mut guard = pending_waiters.lock().unwrap();
        for tx in guard.drain(..) {
            let _ = tx.send(());
        }
    }

    /// Create a detached channel for packing requests (channel-first).
    pub fn channel(bound: usize) -> (PackingSender, PackingReceiver) {
        tokio::sync::mpsc::channel::<PackingRequest>(bound)
    }

    /// Attach a previously created receiver and start the enqueue loop.
    /// Returns a `PackingHandle` for interacting with the service.
    pub fn attach_receiver_loop(
        &self,
        runtime_handle: tokio::runtime::Handle,
        mut rx: PackingReceiver,
        sender: PackingSender,
    ) -> PackingHandle {
        // Clone references to the shared internals so both the service and observers see the same state
        let job_queues = self.pending_jobs.clone();

        // Build an Internals snapshot that shares the same underlying Arcs as the service
        let internals = Internals {
            pending_jobs: self.pending_jobs.clone(),
            semaphore: self.semaphore.clone(),
            config: self.packing_config.clone(),
            active_workers: self.active_workers.clone(),
        };

        // Control channel for waiter/introspection
        let (packing_service_sender, mut ctrl_rx) =
            tokio::sync::mpsc::channel::<PackingServiceMessage>(64);

        // Shared list of pending drain responders
        let pending_waiters: Arc<Mutex<Vec<oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(Vec::new()));

        let flush_if_idle = {
            let internals = internals.clone();
            let pending = pending_waiters.clone();
            move || {
                if Self::is_system_idle(&internals) {
                    Self::flush_waiters(&pending);
                }
            }
        };

        // Spawn a watcher that listens to state-change notifications and flushes pending waiters when idle
        {
            let notify = self.notify.clone();
            let internals = internals.clone();
            let pending = pending_waiters.clone();
            runtime_handle.spawn(async move {
                loop {
                    // Capture Notified future BEFORE checking state to avoid missing notifications
                    let notified = notify.notified();

                    // Check if idle and flush waiters if so
                    if Self::is_system_idle(&internals) {
                        Self::flush_waiters(&pending);
                    }

                    // Wait for next state change
                    notified.await;
                }
            });
        }

        // Spawn the receiver loop handling both job and control channels
        {
            let notify = self.notify.clone();
            let internals_ctrl = internals.clone();
            runtime_handle.spawn(async move {
                loop {
                    tokio::select! {
                        Some(msg) = rx.recv() => {
                            // Enqueue into the SM-specific pending queue; create queue if missing
                            let sm_id = msg.storage_module.id;

                            // HOT path: try read-lock first to avoid write contention when the queue already exists
                            let queue_arc = if let Some(q) = {
                                let map_guard = job_queues
                                    .read()
                                    .expect("Unable to acquire pending jobs map read lock");
                                map_guard.get(&sm_id).cloned()
                            } {
                                q
                            } else {
                                // COLD path: create the queue under a write lock
                                let mut map_guard = job_queues
                                    .write()
                                    .expect("Unable to acquire pending jobs map write lock");
                                map_guard
                                    .entry(sm_id)
                                    .or_insert_with(|| Arc::new(RwLock::new(VecDeque::with_capacity(32))))
                                    .clone()
                            };

                            // Ignore poisoned lock errors by propagating panic (consistent with existing code)
                            queue_arc.as_ref().write().unwrap().push_back(msg);
                            // signal new activity for waiters
                            notify.notify_waiters();
                            // best-effort flush
                            flush_if_idle();
                        },
                        Some(ctrl) = ctrl_rx.recv() => {
                            match ctrl {
                                PackingServiceMessage::Drain { respond_to } => {
                                    if Self::is_system_idle(&internals_ctrl) {
                                        let _ = respond_to.send(());
                                    } else {
                                        pending_waiters.lock().unwrap().push(respond_to);
                                    }
                                }
                            }
                        },
                        else => break,
                    }
                }
            });
        }

        PackingHandle {
            sender,
            internals,

            packing_service_sender,
        }
    }

    /// Spawn a Tokio task that receives PackingRequest messages and pushes them into the internal queues.
    /// Returns a `PackingHandle` for interacting with the service.
    pub fn spawn_tokio_service(&self, runtime_handle: tokio::runtime::Handle) -> PackingHandle {
        let (tx, rx) = tokio::sync::mpsc::channel::<PackingRequest>(5_000);
        self.attach_receiver_loop(runtime_handle, rx, tx)
    }
}

/// waits for any pending & active packing tasks to complete
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

    use crate::packing::{cast_vec_u8_to_vec_u8_array, PackingRequest, PackingService};

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
        let packing = PackingService::new(Arc::new(config.clone()));

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

        // Wait until packing starts to avoid racing the idle waiter
        let wait_start = std::time::Instant::now();
        loop {
            if !storage_module.get_intervals(ChunkType::Entropy).is_empty() {
                break;
            }
            if wait_start.elapsed() > Duration::from_secs(5) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        handle
            .waiter()
            .wait_for_idle(Some(Duration::from_secs(99999)))
            .await?;
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
