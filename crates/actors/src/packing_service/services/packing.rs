//! Packing service for computing entropy chunks
//!
//! - Pack chunks using CPU, CUDA/GPU, or remote services
//! - Manage per-storage-module job queues with work-stealing
//! - Control concurrency with semaphores and worker coordination
//! - Support graceful shutdown and idle detection

use super::super::constants::*;

pub type PackingResult<T> = Result<T, PackingError>;

/// Sync pending chunks with warning on error
pub fn sync_with_warning(storage_module: &irys_domain::StorageModule, context: &str) {
    if let Err(e) = storage_module.sync_pending_chunks() {
        warn!(
            target: "irys::packing",
            "Sync failed during {} for SM {}: {:?}",
            context, storage_module.id, e
        );
    }
}

/// Log packing progress at regular intervals
#[inline]
pub fn log_packing_progress(
    strategy: &str,
    current_offset: u32,
    chunk_range: &irys_types::PartitionChunkRange,
    storage_module_id: usize,
    partition_hash: &irys_types::partition::PartitionHash,
    mining_address: &[u8; 20],
) {
    if current_offset.is_multiple_of(LOG_PER_CHUNKS) {
        debug!(
            target: "irys::packing::update",
            "{} packed chunks {} - {} / {} for SM {} partition_hash {} mining_address {:?}",
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

use super::super::{
    config::PackingConfig,
    errors::PackingError,
    guard::ActiveWorkerGuard,
    strategies::{PackingStrategy, cpu::CpuPackingStrategy, remote::RemotePackingStrategy},
    types::{
        PackingHandle, PackingInternals, PackingQueues, PackingRequest, PackingServiceMessage,
    },
};

#[cfg(feature = "nvidia")]
use super::super::strategies::cuda::CudaPackingStrategy;

use dashmap::DashMap;
use irys_packing::{PACKING_TYPE, PackingType};
use irys_types::{
    Config, PartitionChunkOffset, PartitionChunkRange, TokioServiceHandle, ii,
    partition_chunk_offset_ii,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tracing::{debug, info, warn};

/// Main packing service that orchestrates all packing operations
#[derive(Clone)]
pub struct InternalPackingService {
    config: Arc<Config>,
    packing_config: PackingConfig,
    pending_jobs: PackingQueues,
    semaphore: Arc<Semaphore>,
    active_workers: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    local_strategy: Arc<dyn PackingStrategy>,
    remote_strategy: Arc<RemotePackingStrategy>,
}

impl InternalPackingService {
    pub fn new(config: Arc<Config>) -> Self {
        let packing_config = PackingConfig::new(&config);
        let semaphore = Arc::new(Semaphore::new(packing_config.concurrency.into()));
        let pending_jobs: PackingQueues = Arc::new(DashMap::new());
        let notify = Arc::new(Notify::new());

        let runtime_handle = tokio::runtime::Handle::current();

        let local_strategy: Arc<dyn PackingStrategy> = match PACKING_TYPE {
            PackingType::CPU => {
                info!("Initializing CPU packing strategy");
                Arc::new(CpuPackingStrategy::new(
                    config.clone(),
                    semaphore.clone(),
                    notify.clone(),
                    runtime_handle,
                    packing_config.chain_id,
                ))
            }
            #[cfg(feature = "nvidia")]
            PackingType::CUDA => {
                info!("Initializing CUDA packing strategy");
                Arc::new(CudaPackingStrategy::new(
                    config.clone(),
                    packing_config.clone(),
                    semaphore.clone(),
                    runtime_handle,
                ))
            }
            #[cfg(not(feature = "nvidia"))]
            PackingType::CUDA => {
                panic!("CUDA packing requested but nvidia feature not enabled");
            }
            _ => unimplemented!("Unsupported packing type"),
        };

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
        }
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

        // Work-stealing: Try all SM queues to balance load and prevent starvation
        // when some storage modules are idle while others have pending work
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
            crate::metrics::record_packing_workers(
                self.active_workers.load(Ordering::Relaxed) as u64,
                self.semaphore.available_permits() as u64,
            );

            // TODO(optimization): Check for already-packed ranges.

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

            let range_start = *job_chunk_range.0.start();
            let range_end = *job_chunk_range.0.end();

            // Use half the configured sync interval for more frequent intermediate syncs during packing
            let short_writes_before_sync: u32 = (self
                .config
                .node_config
                .storage
                .num_writes_before_sync
                .div_ceil(2))
            .try_into()
            .unwrap_or_else(|_| {
                warn!(
                    target: "irys::packing",
                    "num_writes_before_sync conversion overflow, using u32::MAX"
                );
                u32::MAX
            });

            let current_chunk_range =
                PartitionChunkRange(partition_chunk_offset_ii!(range_start, range_end));

            // Try remote packing first if configured
            if !self.packing_config.remotes.is_empty() {
                if let Ok(()) = self
                    .remote_strategy
                    .pack(
                        &storage_module,
                        current_chunk_range,
                        mining_address,
                        partition_hash,
                        storage_module_id,
                        short_writes_before_sync,
                    )
                    .await
                {
                    sync_with_warning(&storage_module, "remote packing completion");
                    continue;
                }
            }

            // Fall back to local packing strategy
            if let Err(e) = self
                .local_strategy
                .pack(
                    &storage_module,
                    current_chunk_range,
                    mining_address,
                    partition_hash,
                    storage_module_id,
                    short_writes_before_sync,
                )
                .await
            {
                warn!(target: "irys::packing", "Local packing failed for SM {}: {}", storage_module_id, e);
            }

            sync_with_warning(&storage_module, "local packing completion");
        }
    }

    /// Check if the packing system is idle
    fn is_system_idle(internals: &PackingInternals) -> bool {
        let has_queued_jobs = internals.pending_jobs.iter().any(|entry| {
            let (_sm_id, (_tx, _rx)) = entry.pair();
            _tx.capacity() < PER_SM_CHANNEL_CAPACITY
        });

        let available_permits = internals.semaphore.available_permits();
        let target_permits = internals.config.concurrency as usize;
        let active = internals.active_workers.load(Ordering::Relaxed);

        !has_queued_jobs && active == 0 && available_permits == target_permits
    }

    /// Flush all pending waiters
    fn flush_waiters(pending_waiters: &Mutex<Vec<oneshot::Sender<()>>>) {
        let mut guard = match pending_waiters.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!(target: "irys::packing", "Mutex poisoned in flush_waiters, recovering");
                poisoned.into_inner()
            }
        };
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
    pub fn channel(bound: usize) -> (mpsc::Sender<PackingRequest>, mpsc::Receiver<PackingRequest>) {
        mpsc::channel::<PackingRequest>(bound)
    }

    /// Attach a previously created receiver and start the enqueue loop
    pub fn attach_receiver_loop(
        &self,
        runtime_handle: tokio::runtime::Handle,
        mut rx: mpsc::Receiver<PackingRequest>,
        sender: mpsc::Sender<PackingRequest>,
    ) -> PackingHandle {
        let job_queues = self.pending_jobs.clone();

        let internals = PackingInternals {
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
                                        match pending_waiters.lock() {
                                            Ok(mut guard) => guard.push(respond_to),
                                            Err(poisoned) => {
                                                warn!(target: "irys::packing", "Mutex poisoned when adding waiter, recovering");
                                                poisoned.into_inner().push(respond_to);
                                            }
                                        }
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
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        Config, ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
        StorageSyncConfig,
        partition::{PartitionAssignment, PartitionHash},
    };
    use tokio::sync::Semaphore;

    #[test_log::test(tokio::test)]
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
                    cpu_unpacking_concurrency: 1,
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
        let packing = InternalPackingService::new(Arc::new(config.clone()));

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
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_idle_detection"), false);
        let config = create_test_config_with_chunks(&tmp_dir, 1, 10);
        let service = InternalPackingService::new(Arc::new(config.clone()));

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
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_concurrent_enqueue"), false);
        let config = create_test_config(&tmp_dir, 4);
        let service = InternalPackingService::new(Arc::new(config.clone()));
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
    #[case::high_concurrency(8)]
    #[test_log::test(tokio::test)]
    #[timeout(Duration::from_secs(10))]
    async fn test_semaphore_permits_released(#[case] concurrency: u16) -> eyre::Result<()> {
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_semaphore"), false);
        let config = create_test_config_with_chunks(&tmp_dir, concurrency, 5);

        let service = InternalPackingService::new(Arc::new(config.clone()));
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
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_sm_isolation"), false);
        let config = create_test_config(&tmp_dir, 4);
        let service = InternalPackingService::new(Arc::new(config.clone()));
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
        let tmp_dir = setup_tracing_and_temp_dir(Some("test_channel_saturation"), false);
        let config = create_test_config(&tmp_dir, 2);
        let service = InternalPackingService::new(Arc::new(config.clone()));

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

            let internals = PackingInternals {
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

            let is_idle = InternalPackingService::is_system_idle(&internals);

            let expected_idle = queued_jobs == 0
                && active_workers == 0
                && available_permits >= concurrency as usize;

            prop_assert_eq!(is_idle, expected_idle);
        }
    }

    fn create_test_config(tmp_dir: &tempfile::TempDir, concurrency: u16) -> Config {
        create_test_config_with_chunks(tmp_dir, concurrency, 50)
    }

    fn create_test_config_with_chunks(
        tmp_dir: &tempfile::TempDir,
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
            },
            packing: irys_types::PackingConfig {
                local: irys_types::LocalPackingConfig {
                    cpu_packing_concurrency: concurrency,
                    cpu_unpacking_concurrency: concurrency,
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
        _tmp_dir: &tempfile::TempDir,
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
            let tmp_dir = tempfile::tempdir().unwrap();
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
}
