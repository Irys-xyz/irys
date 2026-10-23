//! Packing service for computing entropy chunks
//!
//! - Pack chunks using CPU, CUDA/GPU, or remote services
//! - Manage per-storage-module job queues with work-stealing
//! - Control concurrency with semaphores and worker coordination
//! - Support graceful shutdown and idle detection

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PackingError {
    #[error("Invalid partition assignment for SM {sm_id}")]
    InvalidAssignment { sm_id: usize },

    #[error("Invalid chunk range: {requested:?} exceeds max {max}")]
    InvalidRange {
        requested: irys_types::PartitionChunkRange,
        max: u64,
    },
}

pub type PackingResult<T> = Result<T, PackingError>;

/// Log progress every N chunks to balance visibility vs log volume
const LOG_PER_CHUNKS: u32 = 1000;

/// Buffer multiplier for remote streaming
const REMOTE_STREAM_BUFFER_MULTIPLIER: u64 = 2;

/// Default capacity for the main packing request channel
const DEFAULT_CHANNEL_CAPACITY: usize = 5_000;

/// Capacity for control message channels
const CONTROL_CHANNEL_CAPACITY: usize = 64;

/// Per-storage-module channel capacity
const PER_SM_CHANNEL_CAPACITY: usize = 100;

/// Sync pending chunks with warning on error
#[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module.id, custom.context = context))]
pub(crate) fn sync_with_warning(storage_module: &irys_domain::StorageModule, context: &str) {
    if let Err(e) = storage_module.sync_pending_chunks() {
        warn!(
            target: "irys::packing",
            "Sync failed during {} for SM {}: {:?}",
            context, storage_module.id, e
        );
    }
}

/// Interval between pending-room checks while packing waits on the disk.
const PENDING_ROOM_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How long packing waits for a module's pending writes to drain.
const PENDING_ROOM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Spans inside `range` that an entropy retry may still pack.
///
/// Empty when the assignment is gone or the hash changed. A range that is
/// still entirely uninitialized comes back as that one span. This is a hint.
/// [`irys_domain::StorageModule::write_entropy_chunk`] decides the write.
pub(crate) fn entropy_target_ranges(
    storage_module: &irys_domain::StorageModule,
    packed_for: irys_types::partition::PartitionHash,
    range: PartitionChunkRange,
) -> Vec<PartitionChunkRange> {
    storage_module
        .entropy_gaps(packed_for, range.0)
        .into_iter()
        .map(PartitionChunkRange)
        .collect()
}

/// Chunks between packing syncs.
///
/// Strategies use this as a modulus. The config value is a `u64`, and half
/// of it rounded up can exceed `u32::MAX`. A wider cadence does not fit in
/// that modulus.
fn short_writes_before_sync(num_writes_before_sync: u64) -> u32 {
    u32::try_from(num_writes_before_sync.div_ceil(2)).unwrap_or(u32::MAX)
}

/// Wake the disk lane, then wait until the module has room for more pending
/// chunks. Packing computes entropy faster than a disk writes it, and with the
/// lane running a sync only wakes the lane, so without this wait the pending
/// queue grows without bound.
///
/// Fails after [`PENDING_ROOM_TIMEOUT`]: a module whose writes keep failing
/// never drains, and the caller must requeue the range instead of holding a
/// packing controller forever.
pub(crate) async fn wait_for_pending_room(
    storage_module: &irys_domain::StorageModule,
    context: &str,
) -> Result<(), String> {
    sync_with_warning(storage_module, context);
    let deadline = std::time::Instant::now() + PENDING_ROOM_TIMEOUT;
    while !storage_module.has_pending_room() {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "SM {} pending writes did not drain within {:?} during {context}",
                storage_module.id, PENDING_ROOM_TIMEOUT
            ));
        }
        tokio::time::sleep(PENDING_ROOM_POLL).await;
        sync_with_warning(storage_module, context);
    }
    Ok(())
}

/// Log packing progress at regular intervals
#[inline]
#[tracing::instrument(level = "trace", skip_all, fields(packing.strategy = strategy, chunk.current_offset = current_offset, storage_module.id = storage_module_id, partition.hash = %partition_hash))]
pub(crate) fn log_packing_progress(
    strategy: &str,
    current_offset: u32,
    chunk_range: &irys_types::PartitionChunkRange,
    storage_module_id: usize,
    partition_hash: &irys_types::partition::PartitionHash,
    mining_address: &IrysAddress,
) {
    if current_offset.is_multiple_of(LOG_PER_CHUNKS) {
        debug!(
            target: "irys::packing::update",
            "{} packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {}",
            strategy,
            chunk_range.0.start(),
            current_offset,
            chunk_range.0.end(),
            storage_module_id,
            partition_hash,
            mining_address
        );
    }
}

mod client_pool;
mod config;
mod guard;
mod strategies;
mod types;

pub use config::PackingConfig;
use guard::ActiveWorkerGuard;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
pub use types::{Internals, PackingQueues, PackingReceiver, PackingSemaphore, PackingSender};
pub use types::{PackingHandle, PackingIdleWaiter, PackingRequest};

use dashmap::DashMap;
use irys_packing::{PACKING_TYPE, PackingType};
use irys_types::{Config, IrysAddress, PartitionChunkRange, TokioServiceHandle};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::metrics;

use self::{
    strategies::{PackingStrategy, cpu::CpuPackingStrategy, remote::RemotePackingStrategy},
    types::PackingServiceMessage,
};

#[cfg(feature = "nvidia")]
use strategies::cuda::CudaPackingStrategy;

/// Main packing service that orchestrates all packing operations
#[derive(Clone)]
pub struct PackingService {
    /// Configuration
    config: Arc<Config>,
    packing_config: PackingConfig,

    /// Job queue management
    pending_jobs: PackingQueues,

    /// Concurrency control
    semaphore: PackingSemaphore,

    /// Worker coordination
    active_workers: Arc<AtomicUsize>,
    notify: Arc<Notify>,

    /// Packing strategies
    local_strategy: Arc<dyn PackingStrategy>,
    remote_strategy: Arc<RemotePackingStrategy>,

    /// Shared with the CPU strategy. Tests pause `pack` after the assignment
    /// is read and before the first write.
    #[cfg(all(test, not(feature = "nvidia")))]
    pack_pause: Arc<Mutex<Option<Arc<strategies::cpu::PackPause>>>>,
}

impl PackingService {
    pub fn new(config: Arc<Config>) -> Self {
        let packing_config = PackingConfig::new(&config);
        let semaphore = Arc::new(Semaphore::new(packing_config.concurrency.into()));
        let pending_jobs: PackingQueues = Arc::new(DashMap::new());
        let notify = Arc::new(Notify::new());

        let runtime_handle = tokio::runtime::Handle::current();
        #[cfg(all(test, not(feature = "nvidia")))]
        let pack_pause = Arc::new(Mutex::new(None));

        // Initialize local packing strategy based on PACKING_TYPE
        let local_strategy: Arc<dyn PackingStrategy> = match PACKING_TYPE {
            PackingType::CPU => {
                info!("Initializing CPU packing strategy");
                Arc::new(CpuPackingStrategy::new(
                    config.clone(),
                    semaphore.clone(),
                    notify.clone(),
                    runtime_handle,
                    packing_config.chain_id,
                    packing_config.concurrency,
                    #[cfg(all(test, not(feature = "nvidia")))]
                    pack_pause.clone(),
                    // No test drives the pause under nvidia.
                    #[cfg(all(test, feature = "nvidia"))]
                    Arc::new(Mutex::new(None)),
                ))
            }
            #[cfg(feature = "nvidia")]
            PackingType::CUDA => {
                // A zero batch cannot be split into ranges. The config default
                // is 0, and `split_interval` rejects that step. Pack on the CPU
                // so the first range does not panic the controller.
                if packing_config.max_chunks == 0 {
                    info!("GPU packing batch size is 0; packing on the CPU");
                    Arc::new(CpuPackingStrategy::new(
                        config.clone(),
                        semaphore.clone(),
                        notify.clone(),
                        runtime_handle,
                        packing_config.chain_id,
                        packing_config.concurrency,
                        #[cfg(test)]
                        Arc::new(Mutex::new(None)),
                    ))
                } else {
                    info!("Initializing CUDA packing strategy");
                    Arc::new(CudaPackingStrategy::new(
                        config.clone(),
                        packing_config.clone(),
                        semaphore.clone(),
                        runtime_handle,
                    ))
                }
            }
            #[cfg(not(feature = "nvidia"))]
            PackingType::CUDA => {
                panic!("CUDA packing requested but nvidia feature not enabled");
            }
            PackingType::AMD => unimplemented!("Unsupported packing type"),
        };

        // Initialize remote packing strategy
        let remote_strategy = Arc::new(RemotePackingStrategy::new(
            config.clone(),
            packing_config.clone(),
            notify.clone(),
        ));

        Self {
            pending_jobs,
            semaphore,
            packing_config,
            config,
            active_workers: Arc::new(AtomicUsize::new(0)),
            notify,
            local_strategy,
            remote_strategy,
            #[cfg(all(test, not(feature = "nvidia")))]
            pack_pause,
        }
    }

    #[cfg(all(test, not(feature = "nvidia")))]
    fn set_pack_pause_for_test(&self, pause: Option<Arc<strategies::cpu::PackPause>>) {
        *self.pack_pause.lock().expect("pack pause") = pause;
    }

    /// Try to poll a job from any non-empty queue, or wait for new work
    async fn poll_next_job(&self) -> Option<PackingRequest> {
        // NOTE: Must capture Notified future BEFORE checking queues to avoid race
        let notified = self.notify.notified();

        // Collect all receiver futures
        let receivers: Vec<_> = self
            .pending_jobs
            .iter()
            .map(|entry| {
                let (_sm_id, (_tx, rx)) = entry.pair();
                rx.clone()
            })
            .collect();

        if receivers.is_empty() {
            tokio::select! {
                _ = notified => {},
                _ = tokio::time::sleep(self.packing_config.poll_duration) => {},
            }
            return None;
        }

        // Try to get a job from any receiver (work-stealing)
        for rx in &receivers {
            if let Ok(req) = rx.lock().await.try_recv() {
                return Some(req);
            }
        }

        // Wait for notification or timeout
        tokio::select! {
            _ = notified => None,
            _ = tokio::time::sleep(self.packing_config.poll_duration) => None,
        }
    }

    /// Main worker loop processing packing jobs
    async fn process_jobs(self) {
        loop {
            let next_range = match self.poll_next_job().await {
                Some(job) => job,
                None => continue,
            };

            let _guard = ActiveWorkerGuard::new(self.active_workers.clone(), self.notify.clone());
            metrics::record_packing_workers(
                self.active_workers.load(Ordering::Relaxed) as u64,
                self.semaphore.available_permits() as u64,
            );

            let storage_module = next_range.storage_module().clone();
            let job_chunk_range = *next_range.chunk_range();

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

            // Re-check labels before this attempt. A retry of the original
            // range must not pack Data, or Entropy that already landed.
            let targets = entropy_target_ranges(&storage_module, partition_hash, job_chunk_range);
            if targets.is_empty() {
                continue;
            }

            let short_writes_before_sync =
                short_writes_before_sync(self.config.node_config.storage.num_writes_before_sync);

            for target in targets {
                // Try remote packing first if configured
                if !self.packing_config.remotes.is_empty()
                    && self
                        .remote_strategy
                        .pack(
                            &storage_module,
                            target,
                            mining_address,
                            partition_hash,
                            storage_module_id,
                            short_writes_before_sync,
                        )
                        .await
                        .is_ok()
                {
                    continue;
                }

                // Fall back to local packing strategy
                if let Err(e) = self
                    .local_strategy
                    .pack(
                        &storage_module,
                        target,
                        mining_address,
                        partition_hash,
                        storage_module_id,
                        short_writes_before_sync,
                    )
                    .await
                {
                    warn!(target: "irys::packing", "Local packing failed for SM {}: {}", storage_module_id, e);
                    // Put back only spans that are still an entropy target,
                    // and only while this job's hash is still assigned. A hash
                    // change has its own pack. Data must not be queued again.
                    // One interval: a partition-sized span is not one checksum
                    // offset per chunk. The storage-module tick sends it.
                    let same_hash = storage_module
                        .partition_assignment()
                        .is_some_and(|current| current.partition_hash == partition_hash);
                    if same_hash {
                        for span in entropy_target_ranges(&storage_module, partition_hash, target) {
                            storage_module.requeue_packing_range(span.0);
                        }
                    }
                }
            }

            sync_with_warning(&storage_module, "local packing completion");
        }
    }

    /// Check if the packing system is idle
    #[tracing::instrument(level = "trace", skip_all, fields(packing.has_queued_jobs, packing.active_workers, packing.available_permits))]
    fn is_system_idle(internals: &Internals) -> bool {
        let has_queued_jobs = internals.pending_jobs.iter().any(|entry| {
            let (_sm_id, (tx, _rx)) = entry.pair();
            tx.capacity() < PER_SM_CHANNEL_CAPACITY
        });

        let available_permits = internals.semaphore.available_permits();
        let target_permits = internals.config.concurrency as usize;
        let active = internals.active_workers.load(Ordering::Relaxed);

        !has_queued_jobs && active == 0 && available_permits == target_permits
    }

    /// Flush all pending waiters
    #[tracing::instrument(level = "trace", skip_all, fields(packing.waiter_count))]
    fn flush_waiters(pending_waiters: &Mutex<Vec<oneshot::Sender<()>>>) {
        let mut guard = pending_waiters.lock().unwrap();
        for tx in guard.drain(..) {
            let _ = tx.send(());
        }
    }

    /// Get or create a channel for the given storage module
    fn get_or_create_channel(
        job_queues: &PackingQueues,
        sm_id: usize,
    ) -> mpsc::Sender<PackingRequest> {
        job_queues
            .entry(sm_id)
            .or_insert_with(|| {
                let (tx, rx) = mpsc::channel(PER_SM_CHANNEL_CAPACITY);
                (tx, Arc::new(tokio::sync::Mutex::new(rx)))
            })
            .value()
            .0
            .clone()
    }

    /// Create a detached channel for packing requests
    pub fn channel(bound: usize) -> (PackingSender, PackingReceiver) {
        mpsc::channel::<PackingRequest>(bound)
    }

    /// Attach a previously created receiver and start the enqueue loop
    pub fn attach_receiver_loop(
        &self,
        runtime_handle: tokio::runtime::Handle,
        mut rx: PackingReceiver,
        sender: PackingSender,
    ) -> PackingHandle {
        let job_queues = self.pending_jobs.clone();

        let internals = Internals {
            pending_jobs: self.pending_jobs.clone(),
            semaphore: self.semaphore.clone(),
            config: self.packing_config.clone(),
            active_workers: self.active_workers.clone(),
        };

        // Control channel for waiter/introspection
        let (packing_service_sender, mut ctrl_rx) =
            mpsc::channel::<PackingServiceMessage>(CONTROL_CHANNEL_CAPACITY);

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

        // Spawn idle monitor
        {
            let notify = self.notify.clone();
            let internals = internals.clone();
            let pending = pending_waiters.clone();
            runtime_handle.spawn(async move {
                loop {
                    let notified = notify.notified();

                    if Self::is_system_idle(&internals) {
                        Self::flush_waiters(&pending);
                    }

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
                            let sm_id = msg.storage_module().id;
                            let tx = Self::get_or_create_channel(&job_queues, sm_id);
                            if let Err(e) = tx.send(msg).await {
                                warn!(target: "irys::packing", "Failed to enqueue job for SM {}: {:?}", sm_id, e);
                            }
                            notify.notify_waiters();
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

        PackingHandle::new(sender, internals, packing_service_sender)
    }

    /// Spawn multiple packing controllers
    pub fn spawn_packing_controllers(
        &self,
        runtime_handle: tokio::runtime::Handle,
    ) -> Vec<TokioServiceHandle> {
        let mut handles = Vec::new();
        let controller_count = std::cmp::max(1_usize, self.packing_config.concurrency as usize);

        for i in 0..controller_count {
            let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
            let self_clone = self.clone();

            let handle = runtime_handle.spawn(async move {
                tokio::select! {
                    _ = Self::process_jobs(self_clone) => {},
                    _ = shutdown_rx => {
                        tracing::info!("Packing controller {} received shutdown signal", i);
                    }
                }
            });

            handles.push(TokioServiceHandle {
                name: format!("packing_controller_{}", i),
                handle,
                shutdown_signal: shutdown_tx,
            });
        }

        handles
    }

    /// Spawn a Tokio service that receives PackingRequest messages
    pub fn spawn_tokio_service(&self, runtime_handle: tokio::runtime::Handle) -> PackingHandle {
        let (tx, rx) = mpsc::channel::<PackingRequest>(DEFAULT_CHANNEL_CAPACITY);
        self.attach_receiver_loop(runtime_handle, rx, tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, atomic::AtomicUsize},
        time::Duration,
    };

    use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_packing::capacity_single::compute_entropy_chunk;
    use irys_storage::ie;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
        StorageSyncConfig, ii,
        partition::{PartitionAssignment, PartitionHash},
    };
    use tokio::sync::Semaphore;

    #[test]
    fn short_writes_before_sync_saturates_past_u32() {
        assert_eq!(short_writes_before_sync(1), 1);
        assert_eq!(short_writes_before_sync(100), 50);
        assert_eq!(short_writes_before_sync(101), 51);
        // 2 * u32::MAX still fits. One more does not.
        assert_eq!(short_writes_before_sync(8_589_934_590), u32::MAX);
        assert_eq!(short_writes_before_sync(8_589_934_591), u32::MAX);
        assert_eq!(short_writes_before_sync(u64::MAX), u32::MAX);
    }

    #[test_log::test(tokio::test)]
    async fn panicked_pack_worker_requeues_the_range() -> eyre::Result<()> {
        // A chunk smaller than the entropy hash width makes the CPU worker
        // panic inside `spawn_blocking`. The join must fail the pack, and the
        // service must put the range back.
        let tmp_dir = TempDirBuilder::new()
            .prefix("panicked_pack_requeue")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 16,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), "hdd0".into())],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 0)),
        )?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        handle.send(request)?;

        let start = std::time::Instant::now();
        let queued = loop {
            let ranges = storage_module.take_packing_ranges();
            if !ranges.is_empty() {
                break ranges;
            }
            if start.elapsed() > Duration::from_secs(5) {
                eyre::bail!("a panicked packing worker must put the range back");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            queued,
            vec![ii(PartitionChunkOffset(0), PartitionChunkOffset(0))]
        );
        assert_eq!(
            storage_module.crc_repair_len(),
            0,
            "a failed pack is not a checksum repair"
        );
        Ok(())
    }

    /// The CUDA build's default GPU batch size is 0. That step cannot be
    /// split. The controller must still pack the range on the CPU.
    #[cfg(feature = "nvidia")]
    #[test_log::test(tokio::test)]
    async fn zero_gpu_batch_packs_on_cpu() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new().prefix("zero_gpu_batch").build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        node_config.packing.local.gpu_packing_batch_size = 0;
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), submodule_dir)],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 0)),
        )?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        handle.send(request)?;

        let start = std::time::Instant::now();
        loop {
            if !storage_module.get_intervals(ChunkType::Entropy).is_empty() {
                break;
            }
            if start.elapsed() > Duration::from_secs(10) {
                eyre::bail!("a zero GPU batch must still pack the range");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            storage_module.get_intervals(ChunkType::Entropy),
            vec![ie(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(0)
            )]
        );
        Ok(())
    }

    /// The assignment is read before `pack`. Clearing it while `pack` is
    /// paused must not put the range back: the module can no longer pack.
    #[cfg(not(feature = "nvidia"))]
    #[test_log::test(tokio::test)]
    async fn unassigned_during_pack_does_not_requeue() -> eyre::Result<()> {
        use std::sync::Arc;

        use super::strategies::cpu::PackPause;
        use irys_domain::ChunkType;
        use tokio::sync::Notify;

        let tmp_dir = TempDirBuilder::new()
            .prefix("unassigned_pack_requeue")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        // Absolute. Assignment writes join this path, not the process cwd.
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), submodule_dir)],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 0)),
        )?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        let pause = Arc::new(PackPause {
            started: Notify::new(),
            release: Notify::new(),
            fail_with: std::sync::Mutex::new(None),
        });
        let started = pause.started.notified();
        tokio::pin!(started);
        assert!(
            !started.as_mut().enable(),
            "the pause must not already be notified"
        );
        packing.set_pack_pause_for_test(Some(Arc::clone(&pause)));
        handle.send(request)?;

        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .expect("pack starts while the module is still assigned");
        storage_module.reset_with_assignment(None, 3)?;
        assert!(storage_module.partition_assignment().is_none());
        pause.release.notify_one();
        handle
            .waiter()
            .wait_for_idle(Some(Duration::from_secs(10)))
            .await?;

        assert_ne!(
            storage_module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy),
            "the write must be refused after the module is unassigned"
        );
        let ranges = storage_module.take_crc_repair_ranges();
        assert!(
            ranges.is_empty(),
            "an unassigned module must not requeue the failed pack, still queued: {ranges:?}"
        );
        let packs = storage_module.take_packing_ranges();
        assert!(
            packs.is_empty(),
            "an unassigned module must not queue another pack, still queued: {packs:?}"
        );
        Ok(())
    }

    /// The hash changed while the pack was paused. The failed job must not
    /// come back: the new assignment has its own pack, and this job was for
    /// the previous hash.
    #[cfg(not(feature = "nvidia"))]
    #[test_log::test(tokio::test)]
    async fn reassigned_during_pack_does_not_requeue_the_old_range() -> eyre::Result<()> {
        use std::sync::Arc;

        use super::strategies::cpu::PackPause;
        use irys_domain::ChunkType;
        use tokio::sync::Notify;

        let tmp_dir = TempDirBuilder::new()
            .prefix("reassigned_pack_requeue")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), submodule_dir)],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 0)),
        )?;

        let packing = PackingService::new(Arc::new(config.clone()));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        let pause = Arc::new(PackPause {
            started: Notify::new(),
            release: Notify::new(),
            fail_with: std::sync::Mutex::new(None),
        });
        let started = pause.started.notified();
        tokio::pin!(started);
        assert!(
            !started.as_mut().enable(),
            "the pause must not already be notified"
        );
        packing.set_pack_pause_for_test(Some(Arc::clone(&pause)));
        handle.send(request)?;

        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .expect("pack starts while the module still has the old hash");
        let new_hash = PartitionHash::repeat_byte(0x44);
        storage_module.reset_with_assignment(
            Some(PartitionAssignment {
                partition_hash: new_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            4,
        )?;
        assert_eq!(
            storage_module
                .partition_assignment()
                .expect("still assigned")
                .partition_hash,
            new_hash
        );
        pause.release.notify_one();
        handle
            .waiter()
            .wait_for_idle(Some(Duration::from_secs(10)))
            .await?;

        assert_ne!(
            storage_module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy),
            "the old job must not write entropy for the new hash"
        );
        let ranges = storage_module.take_crc_repair_ranges();
        assert!(
            ranges.is_empty(),
            "a hash change must not requeue the old pack, still queued: {ranges:?}"
        );
        let packs = storage_module.take_packing_ranges();
        assert!(
            packs.is_empty(),
            "a hash change must not queue another pack of the old span, still queued: {packs:?}"
        );
        Ok(())
    }

    /// The first filter includes a whole uninitialized range. Data written
    /// while that pack is paused stays data. A live `Interrupted` offset is
    /// not a pack target. The failure puts back only the offsets that are
    /// still `Uninitialized`.
    #[cfg(not(feature = "nvidia"))]
    #[test_log::test(tokio::test)]
    async fn failed_pack_requeues_only_uninitialized_offsets() -> eyre::Result<()> {
        use std::collections::BTreeSet;

        use super::strategies::cpu::PackPause;
        use tokio::sync::Notify;

        let tmp_dir = TempDirBuilder::new()
            .prefix("requeue_uninitialized_only")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), submodule_dir)],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 3)),
        )?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        let pause = Arc::new(PackPause {
            started: Notify::new(),
            release: Notify::new(),
            fail_with: std::sync::Mutex::new(None),
        });
        let started = pause.started.notified();
        tokio::pin!(started);
        assert!(
            !started.as_mut().enable(),
            "the pause must not already be notified"
        );
        packing.set_pack_pause_for_test(Some(Arc::clone(&pause)));
        handle.send(request)?;
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .expect("pack starts on the range the first filter accepted");

        assert!(storage_module.write_chunk(
            PartitionChunkOffset(0),
            vec![0xD4_u8; 32],
            ChunkType::Data,
        ));
        storage_module.force_sync_pending_chunks()?;
        {
            let mut intervals = storage_module.intervals().write().expect("intervals");
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                PartitionChunkOffset(1),
                ChunkType::Interrupted,
            );
        }
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Data)
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Interrupted)
        );

        *pause.fail_with.lock().expect("fail flag") =
            Some("stopped after the prefix changed".to_string());
        pause.release.notify_one();

        let start = std::time::Instant::now();
        let mut queued = BTreeSet::new();
        loop {
            for range in storage_module.take_packing_ranges() {
                let mut offset = range.start().0;
                loop {
                    queued.insert(offset);
                    if offset == range.end().0 {
                        break;
                    }
                    offset = offset.saturating_add(1);
                }
            }
            if queued == BTreeSet::from([2, 3]) {
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                eyre::bail!("requeued offsets were {queued:?}, want 2 and 3");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            storage_module.crc_repair_len(),
            0,
            "uninitialized spans are not checksum repairs"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Data),
            "the data prefix stays data"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Interrupted),
            "the failed pack must not consume the interrupted offset"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(2)),
            Some(ChunkType::Uninitialized)
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(3)),
            Some(ChunkType::Uninitialized)
        );
        Ok(())
    }

    /// A failed pack of a wide uninitialized span queues that span once.
    /// It does not insert one checksum repair per chunk. A later request
    /// packs the same span.
    #[cfg(not(feature = "nvidia"))]
    #[test_log::test(tokio::test)]
    async fn failed_pack_of_a_large_span_requeues_one_range() -> eyre::Result<()> {
        use super::strategies::cpu::PackPause;
        use tokio::sync::Notify;

        const SPAN_END: u32 = 4_095;

        let tmp_dir = TempDirBuilder::new()
            .prefix("large_span_pack_requeue")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: u64::from(SPAN_END) + 1,
            chunk_size: 32,
            ..consensus
        });
        node_config.storage.num_writes_before_sync = 32;
        node_config.storage.min_durable_interval_ms = 0;
        node_config.packing.local.cpu_packing_concurrency = 1;
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                irys_types::partition_chunk_offset_ie!(0, 4096),
                submodule_dir,
            )],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        let span = ii(PartitionChunkOffset(0), PartitionChunkOffset(SPAN_END));
        let request = PackingRequest::new(storage_module.clone(), PartitionChunkRange(span))?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        let pause = Arc::new(PackPause {
            started: Notify::new(),
            release: Notify::new(),
            fail_with: std::sync::Mutex::new(None),
        });
        let started = pause.started.notified();
        tokio::pin!(started);
        assert!(
            !started.as_mut().enable(),
            "the pause must not already be notified"
        );
        packing.set_pack_pause_for_test(Some(Arc::clone(&pause)));
        handle.send(request)?;
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .expect("pack starts on the uninitialized span");

        *pause.fail_with.lock().expect("fail flag") = Some("disk lane stayed full".to_string());
        pause.release.notify_one();

        let start = std::time::Instant::now();
        let queued = loop {
            let repairs = storage_module.crc_repair_len();
            if repairs != 0 {
                eyre::bail!(
                    "a failed pack of {} chunks inserted {repairs} checksum repairs",
                    u64::from(SPAN_END) + 1
                );
            }
            let queued = storage_module.take_packing_ranges();
            if !queued.is_empty() {
                break queued;
            }
            if start.elapsed() > Duration::from_secs(5) {
                eyre::bail!("a failed pack did not queue the span");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(queued, vec![span], "the span is one range");
        assert_eq!(
            storage_module.crc_repair_len(),
            0,
            "the checksum set stays empty"
        );

        packing.set_pack_pause_for_test(None);
        handle.send(PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(span),
        )?)?;

        let start = std::time::Instant::now();
        loop {
            let entropy = storage_module.get_intervals(ChunkType::Entropy);
            if entropy.len() == 1 && *entropy[0].start() == 0 && *entropy[0].end() == SPAN_END {
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                eyre::bail!("the requeued span was not packed, entropy={entropy:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(storage_module.crc_repair_len(), 0);
        assert!(storage_module.take_packing_ranges().is_empty());
        Ok(())
    }

    /// A file `Interrupted` reloads as `Uninitialized` and is packed.
    /// `Uninitialized` is packed. A live `Interrupted` label is the in-flight
    /// durable-point mark and is not packed. A neighboring `Data` chunk stays.
    #[cfg(not(feature = "nvidia"))]
    #[test_log::test(tokio::test)]
    async fn interrupted_label_is_packed() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("interrupted_label_packed")
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
            chunk_size: 32,
            ..consensus
        });
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = PartitionHash::zero();
        let submodule_dir = tmp_dir.path().join("hdd0");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), submodule_dir)],
        };
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        assert!(storage_module.write_chunk(
            PartitionChunkOffset(1),
            vec![0xD4_u8; 32],
            ChunkType::Data,
        ));
        storage_module.force_sync_pending_chunks()?;
        {
            let mut intervals = storage_module.intervals().write().expect("intervals");
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                PartitionChunkOffset(0),
                ChunkType::Interrupted,
            );
        }
        storage_module.write_intervals_to_submodules()?;
        drop(storage_module);
        let storage_module = Arc::new(StorageModule::new(&info, &config)?);
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized),
            "a reloaded Interrupted label is Uninitialized"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Data)
        );
        {
            let mut intervals = storage_module.intervals().write().expect("intervals");
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                PartitionChunkOffset(2),
                ChunkType::Interrupted,
            );
        }
        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 3)),
        )?;

        let packing = PackingService::new(Arc::new(config));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = packing.spawn_packing_controllers(runtime_handle.clone());
        let handle = packing.spawn_tokio_service(runtime_handle);
        handle.send(request)?;

        let start = std::time::Instant::now();
        loop {
            let reloaded = storage_module.get_chunk_type(&PartitionChunkOffset(0));
            let fresh = storage_module.get_chunk_type(&PartitionChunkOffset(3));
            if reloaded == Some(ChunkType::Entropy) && fresh == Some(ChunkType::Entropy) {
                break;
            }
            if start.elapsed() > Duration::from_secs(10) {
                eyre::bail!("reloaded={reloaded:?} fresh={fresh:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Data),
            "a successful pack must not overwrite data"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset(2)),
            Some(ChunkType::Interrupted),
            "a live Interrupted label is not a pack target"
        );
        let ranges = storage_module.take_crc_repair_ranges();
        assert!(
            ranges.is_empty(),
            "a successful pack must not requeue, still queued: {ranges:?}"
        );
        let packs = storage_module.take_packing_ranges();
        assert!(
            packs.is_empty(),
            "a successful pack must not queue another pack, still queued: {packs:?}"
        );
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn test_packing_actor() -> eyre::Result<()> {
        // setup
        let partition_hash = PartitionHash::zero();
        let num_chunks = 50;
        let to_pack = 10;
        let packing_end = num_chunks - to_pack;

        let tmp_dir = TempDirBuilder::new()
            .prefix("test_packing_actor")
            .with_tracing()
            .build();
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
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
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
        let config = Config::new_with_random_peer_id(node_config);

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

        let request = PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(irys_types::partition_chunk_offset_ie!(0, packing_end)),
        )?;
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

    // ========================================================================
    // Race Condition & Concurrency Tests
    // ========================================================================
    #[rstest::rstest]
    #[case::already_idle(true)]
    #[case::becomes_idle(false)]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(5))]
    async fn test_idle_detection_no_lost_notifications(
        #[case] start_idle: bool,
    ) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("test_idle_detection")
            .with_tracing()
            .build();
        let config = create_test_config_with_chunks(&tmp_dir, 1, 10);
        let service = PackingService::new(Arc::new(config.clone()));

        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = service.spawn_packing_controllers(runtime_handle.clone());
        let handle = service.spawn_tokio_service(runtime_handle);

        if !start_idle {
            let storage_module = create_test_storage_module(&config, &tmp_dir, 0)?;
            let req = PackingRequest::new(
                storage_module.clone(),
                PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(5))),
            )?;
            handle.send(req)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let result = handle
            .waiter()
            .wait_for_idle(Some(Duration::from_secs(3)))
            .await;
        assert!(result.is_ok(), "Idle detection should not hang");
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn test_concurrent_job_enqueue_no_lost_jobs() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("test_concurrent_enqueue")
            .with_tracing()
            .build();
        let config = create_test_config(&tmp_dir, 4);
        let service = PackingService::new(Arc::new(config.clone()));
        let handle = service.spawn_tokio_service(tokio::runtime::Handle::current());

        let job_count = 100;
        let num_sms = 5;

        let storage_modules: Vec<_> = (0..num_sms)
            .map(|i| create_test_storage_module(&config, &tmp_dir, i))
            .collect::<Result<_, _>>()?;

        let handles: Vec<_> = (0..job_count)
            .map(|i| {
                let h = handle.clone();
                let sm = storage_modules[i % num_sms].clone();
                tokio::spawn(async move {
                    let req = PackingRequest::new(
                        sm,
                        PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
                    )
                    .unwrap();
                    h.send(req).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.await?;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        let internals = handle.internals();
        // Note: With channels, we can't directly count queued items without draining
        // Instead, verify channels were created for the expected number of SMs
        let num_channels = internals.pending_jobs.len();
        assert!(
            num_channels > 0 && num_channels <= num_sms,
            "Should have channels for SMs, got {}",
            num_channels
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::single_permit(1)]
    #[case::multiple_permits(4)]
    #[case::heavy3_high_concurrency(8)]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(10))]
    async fn heavy4_test_semaphore_permits_released(#[case] concurrency: u16) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("test_semaphore")
            .with_tracing()
            .build();
        let config = create_test_config_with_chunks(&tmp_dir, concurrency, 5);

        let service = PackingService::new(Arc::new(config.clone()));
        let runtime_handle = tokio::runtime::Handle::current();
        let _controllers = service.spawn_packing_controllers(runtime_handle.clone());
        let handle = service.spawn_tokio_service(runtime_handle);

        let storage_module = create_test_storage_module(&config, &tmp_dir, 0)?;

        for _ in 0..(concurrency as usize * 2) {
            let req = PackingRequest::new(
                storage_module.clone(),
                PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(4))),
            )?;
            handle.send(req)?;
        }

        handle
            .waiter()
            .wait_for_idle(Some(Duration::from_secs(8)))
            .await
            .expect("Should not deadlock on permit exhaustion");

        let internals = handle.internals();
        assert_eq!(
            internals.semaphore.available_permits(),
            concurrency as usize,
            "All permits should be released"
        );
        Ok(())
    }

    // ========================================================================
    // Storage Module Isolation Tests
    // ========================================================================

    #[rstest::rstest]
    #[case::single_sm(vec![0])]
    #[case::two_sms(vec![0, 1])]
    #[case::many_sms(vec![0, 1, 2, 3, 4])]
    #[test_log::test(tokio::test)]
    async fn test_storage_module_queue_isolation(#[case] sm_ids: Vec<usize>) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("test_sm_isolation")
            .with_tracing()
            .build();
        let config = create_test_config(&tmp_dir, 4);
        let service = PackingService::new(Arc::new(config.clone()));
        let handle = service.spawn_tokio_service(tokio::runtime::Handle::current());

        let storage_modules: Vec<_> = sm_ids
            .iter()
            .map(|&id| create_test_storage_module(&config, &tmp_dir, id))
            .collect::<Result<_, _>>()?;

        for sm in &storage_modules {
            let req = PackingRequest::new(
                sm.clone(),
                PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
            )?;
            handle.send(req)?;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;

        let internals = handle.internals();
        let num_channels = internals.pending_jobs.len();
        assert_eq!(
            num_channels,
            sm_ids.len(),
            "Should have separate channel per SM"
        );

        for &sm_id in &sm_ids {
            assert!(
                internals.pending_jobs.contains_key(&sm_id),
                "Channel should exist for SM {}",
                sm_id
            );
        }
        Ok(())
    }

    // ========================================================================
    // Channel Saturation Tests
    // ========================================================================

    #[test_log::test(tokio::test)]
    async fn test_channel_saturation_drops_jobs_gracefully() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("test_channel_saturation")
            .with_tracing()
            .build();
        let config = create_test_config(&tmp_dir, 2);
        let service = PackingService::new(Arc::new(config.clone()));

        let (tx, rx) = tokio::sync::mpsc::channel::<PackingRequest>(2);
        let handle = service.attach_receiver_loop(tokio::runtime::Handle::current(), rx, tx);

        let storage_module = create_test_storage_module(&config, &tmp_dir, 0)?;

        handle.send(PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
        )?)?;
        handle.send(PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
        )?)?;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = handle.send(PackingRequest::new(
            storage_module.clone(),
            PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
        )?);

        assert!(result.is_ok(), "Saturated send should not error, just drop");
        Ok(())
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]
        #[test]
        fn test_is_system_idle_invariant(
            queued_jobs in 0..100_usize,
            active_workers in 0..20_usize,
            available_permits in 0..20_usize,
            concurrency in 1..20_u16,
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let pending_jobs = Arc::new(DashMap::new());

            if queued_jobs > 0 {
                // Create a channel with items in it
                let (tx, rx) = tokio::sync::mpsc::channel(PER_SM_CHANNEL_CAPACITY);
                rt.block_on(async {
                    for _ in 0..queued_jobs.min(PER_SM_CHANNEL_CAPACITY) {
                        tx.send(create_dummy_request()).await.unwrap();
                    }
                });
                pending_jobs.insert(0, (tx, Arc::new(tokio::sync::Mutex::new(rx))));
            }

            let semaphore = Arc::new(Semaphore::new(concurrency as usize));
            for _ in 0..(concurrency as usize - available_permits.min(concurrency as usize)) {
                semaphore.try_acquire().unwrap().forget();
            }

            let internals = Internals {
                pending_jobs,
                semaphore,
                active_workers: Arc::new(AtomicUsize::new(active_workers)),
                config: PackingConfig {
                    poll_duration: Duration::from_millis(100),
                    concurrency,
                    chain_id: 1,
                    #[cfg(feature = "nvidia")]
                    max_chunks: 100,
                    remotes: vec![],
                },
            };

            let is_idle = PackingService::is_system_idle(&internals);

            let expected_idle = queued_jobs == 0
                && active_workers == 0
                && available_permits >= concurrency as usize;

            prop_assert_eq!(is_idle, expected_idle);
        }
    }

    /// A one-offset retry must not walk every interval fragment. Pending
    /// data inside that offset is not a pack target. A fresh module still
    /// returns its uninitialized span.
    #[test]
    fn entropy_target_ranges_reads_only_the_overlapping_fragments() -> eyre::Result<()> {
        const CHUNKS: u64 = 64;
        let tmp_dir = TempDirBuilder::new().prefix("entropy_range_scan").build();
        let config = create_test_config_with_chunks(&tmp_dir, 1, CHUNKS);
        let module = create_test_storage_module(&config, &tmp_dir, 0)?;
        let hash = module
            .partition_assignment()
            .expect("assigned")
            .partition_hash;
        let full = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 63));
        let all = entropy_target_ranges(&module, hash, full);
        assert_eq!(all.len(), 1, "a fresh module is one uninitialized span");
        assert_eq!(all[0].0.start().0, 0);
        assert_eq!(all[0].0.end().0, 63);

        for offset in (0..CHUNKS as u32).filter(|offset| offset % 2 == 0) {
            assert!(
                module.write_chunk(
                    PartitionChunkOffset(offset),
                    vec![0xE1_u8; 32],
                    ChunkType::Entropy,
                ),
                "entropy at {offset}"
            );
        }
        module.force_sync_pending_chunks()?;
        assert_eq!(
            module.get_intervals(ChunkType::Entropy).len(),
            32,
            "even offsets did not become separate entropy spans"
        );

        let one = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(1, 1));
        let before = module.interval_visit_count();
        let targets = entropy_target_ranges(&module, hash, one);
        let visits = module.interval_visit_count() - before;
        assert_eq!(targets.len(), 1, "offset 1 stays uninitialized");
        assert_eq!(targets[0].0.start().0, 1);
        assert_eq!(targets[0].0.end().0, 1);
        assert!(visits <= 2, "one-offset query visited {visits} intervals");

        assert!(
            module.write_chunk(PartitionChunkOffset(1), vec![0xD4_u8; 32], ChunkType::Data,),
            "pending data"
        );
        let pending = entropy_target_ranges(&module, hash, one);
        assert!(
            pending.is_empty(),
            "pending data inside the range is still a pack target"
        );
        let neighbor = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(3, 3));
        let neighbor_targets = entropy_target_ranges(&module, hash, neighbor);
        assert_eq!(neighbor_targets.len(), 1);
        assert_eq!(neighbor_targets[0].0.start().0, 3);
        Ok(())
    }

    fn create_test_config(
        tmp_dir: &irys_testing_utils::tempfile::TempDir,
        concurrency: u16,
    ) -> Config {
        create_test_config_with_chunks(tmp_dir, concurrency, 50)
    }

    fn create_test_config_with_chunks(
        tmp_dir: &irys_testing_utils::tempfile::TempDir,
        concurrency: u16,
        num_chunks: u64,
    ) -> Config {
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                entropy_packing_iterations: 100000,
                num_chunks_in_partition: num_chunks,
                chunk_size: 32,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 10,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            },
            packing: irys_types::PackingConfig {
                local: irys_types::LocalPackingConfig {
                    cpu_packing_concurrency: concurrency,
                    gpu_packing_batch_size: 10,
                },
                remote: Default::default(),
            },
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        Config::new_with_random_peer_id(node_config)
    }

    fn create_test_storage_module(
        config: &Config,
        _tmp_dir: &irys_testing_utils::tempfile::TempDir,
        id: usize,
    ) -> eyre::Result<Arc<StorageModule>> {
        let partition_hash = PartitionHash::zero();
        let info = StorageModuleInfo {
            id,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                ie(
                    PartitionChunkOffset(0),
                    PartitionChunkOffset(config.consensus.num_chunks_in_partition as u32),
                ),
                format!("hdd{}", id).into(),
            )],
        };

        Ok(Arc::new(StorageModule::new(&info, config)?))
    }

    fn create_dummy_request() -> PackingRequest {
        use std::sync::OnceLock;
        static DUMMY_SM: OnceLock<Arc<StorageModule>> = OnceLock::new();

        let sm = DUMMY_SM.get_or_init(|| {
            let tmp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
            let config = create_test_config(&tmp_dir, 1);
            let sm = create_test_storage_module(&config, &tmp_dir, 999).unwrap();
            std::mem::forget(tmp_dir);
            sm
        });

        PackingRequest::new(
            sm.clone(),
            PartitionChunkRange(ie(PartitionChunkOffset(0), PartitionChunkOffset(10))),
        )
        .unwrap()
    }

    /// `wait_for_pending_room` must stay pending while the module is at its
    /// byte ceiling, then return once a later sync drops below that ceiling.
    ///
    /// The disk lane is stopped. A non-force sync parks the bytes when the
    /// durable interval has not elapsed and the backlog is not over the
    /// ceiling. Headroom at 1000 permille keeps `pending == ceiling` inside
    /// that line, so the wait's own sync does not clear the queue.
    #[test_log::test(tokio::test)]
    async fn pending_room_wait_stays_blocked_until_the_ceiling_clears() -> eyre::Result<()> {
        const CHUNK: u64 = 32;
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_room_wait")
            .with_tracing()
            .build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: CHUNK,
            num_chunks_in_partition: 4,
            ..consensus
        });
        node_config.storage = StorageSyncConfig {
            num_writes_before_sync: 1,
            max_pending_write_bytes: Some(CHUNK),
            min_durable_interval_ms: 60_000,
            durable_headroom_permille: 1000,
            ..StorageSyncConfig::default()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: PartitionHash::zero(),
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                ie(PartitionChunkOffset(0), PartitionChunkOffset(4)),
                "hdd0".into(),
            )],
        };
        let module = Arc::new(StorageModule::new(&info, &config)?);
        assert!(
            module.write_chunk(
                PartitionChunkOffset(0),
                vec![1_u8; CHUNK as usize],
                ChunkType::Entropy,
            ),
            "seed write"
        );
        module.force_sync_pending_chunks()?;
        assert_eq!(
            module.pending_write_bytes(),
            0,
            "the first point must drain"
        );
        assert!(
            module.write_chunk(
                PartitionChunkOffset(1),
                vec![2_u8; CHUNK as usize],
                ChunkType::Entropy,
            ),
            "ceiling write"
        );
        assert!(
            !module.has_pending_room(),
            "one chunk at the ceiling leaves no pending room, pending={} ceiling={}",
            module.pending_write_bytes(),
            module.pending_memory_ceiling_bytes()
        );

        let waited = Arc::clone(&module);
        let waiting =
            tokio::spawn(async move { wait_for_pending_room(&waited, "pending-room test").await });

        // The queued chunk stays Uninitialized until the wait's first sync
        // pwrites it. Entropy plus a full ceiling means that sync parked the
        // bytes and the poll loop is the only way the wait is still running.
        let started = std::time::Instant::now();
        loop {
            if waiting.is_finished() {
                let result = waiting.await.expect("wait task");
                eyre::bail!("wait returned while the module was still at the ceiling: {result:?}");
            }
            let parked = module.get_chunk_type(&PartitionChunkOffset(1))
                == Some(ChunkType::Entropy)
                && !module.has_pending_room();
            if parked {
                break;
            }
            if started.elapsed() > Duration::from_secs(2) {
                eyre::bail!(
                    "the first sync did not park the ceiling chunk, type={:?} pending={}",
                    module.get_chunk_type(&PartitionChunkOffset(1)),
                    module.pending_write_bytes()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // One poll is 20 ms. The wait must still be pending after that sleep.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !waiting.is_finished(),
            "wait returned after a poll while the ceiling was still full"
        );
        assert!(!module.has_pending_room());

        module.force_sync_pending_chunks()?;
        assert!(
            module.has_pending_room(),
            "force sync must drop the parked bytes under the ceiling"
        );
        let joined = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("wait did not return after the ceiling was cleared");
        let result = joined.expect("wait task panicked");
        assert!(result.is_ok(), "{result:?}");
        Ok(())
    }
}
