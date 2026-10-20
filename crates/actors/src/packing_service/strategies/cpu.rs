use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(test)]
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use irys_domain::{EntropyRejection, EntropyWrite, StorageModule};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_types::IrysAddress;
use irys_types::{Config, PartitionChunkOffset, PartitionChunkRange, partition::PartitionHash};
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, yield_now};
use tracing::{debug, trace, warn};

/// Test gate. `pack` notifies `started`, then waits until `release` is
/// notified. `notify_one` stores a permit, so the release can land first.
/// `fail_with` is read after the wait and before any chunk is hashed.
#[cfg(test)]
pub(crate) struct PackPause {
    pub started: Notify,
    pub release: Notify,
    pub fail_with: Mutex<Option<String>>,
}

/// Blocks the entropy write. The blocking packer sets `arrived`, then waits
/// until `release`. The clock is monotonic.
#[cfg(test)]
pub(crate) struct WriteGate {
    inner: Mutex<WriteGateState>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
struct WriteGateState {
    arrived: bool,
    release: bool,
}

#[cfg(test)]
impl WriteGate {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(WriteGateState {
                arrived: false,
                release: false,
            }),
            cv: std::sync::Condvar::new(),
        }
    }

    pub(crate) fn wait_turn(&self) {
        let mut state = self.inner.lock().expect("write gate");
        state.arrived = true;
        self.cv.notify_all();
        while !state.release {
            state = self.cv.wait(state).expect("write gate");
        }
    }

    pub(crate) fn wait_arrived(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.inner.lock().expect("write gate");
        while !state.arrived {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, wait) = self
                .cv
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .expect("write gate");
            state = guard;
            if wait.timed_out() && !state.arrived {
                return false;
            }
        }
        true
    }

    pub(crate) fn release(&self) {
        let mut state = self.inner.lock().expect("write gate");
        state.release = true;
        self.cv.notify_all();
    }
}

/// CPU-based packing strategy
pub(crate) struct CpuPackingStrategy {
    chunk_size: usize,
    entropy_iterations: u32,
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    runtime_handle: tokio::runtime::Handle,
    chain_id: u64,
    /// Same width as the packing semaphore. Join handles stay inside this cap.
    /// A full partition is tens of millions of chunks; retaining one handle per
    /// chunk keeps every finished task alive until the range returns.
    concurrency: u16,
    #[cfg(test)]
    peak_retained_joins: AtomicUsize,
    #[cfg(test)]
    chunks_started: AtomicUsize,
    #[cfg(test)]
    pack_pause: Arc<Mutex<Option<Arc<PackPause>>>>,
    #[cfg(test)]
    write_gate: Arc<Mutex<Option<Arc<WriteGate>>>>,
}

impl CpuPackingStrategy {
    pub(crate) fn new(
        config: Arc<Config>,
        semaphore: Arc<Semaphore>,
        notify: Arc<Notify>,
        runtime_handle: tokio::runtime::Handle,
        chain_id: u64,
        concurrency: u16,
        #[cfg(test)] pack_pause: Arc<Mutex<Option<Arc<PackPause>>>>,
    ) -> Self {
        Self {
            chunk_size: config.consensus.chunk_size as usize,
            entropy_iterations: config.consensus.entropy_packing_iterations,
            semaphore,
            notify,
            runtime_handle,
            chain_id,
            concurrency,
            #[cfg(test)]
            peak_retained_joins: AtomicUsize::new(0),
            #[cfg(test)]
            chunks_started: AtomicUsize::new(0),
            #[cfg(test)]
            pack_pause,
            #[cfg(test)]
            write_gate: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    fn set_write_gate(&self, gate: Option<Arc<WriteGate>>) {
        *self.write_gate.lock().expect("write gate") = gate;
    }

    #[cfg(test)]
    fn note_retained(&self, len: usize) {
        self.peak_retained_joins.fetch_max(len, Ordering::Relaxed);
    }

    /// `Ok` join handles are small, but the task stays allocated until the
    /// handle is awaited. Drain the oldest so a long range cannot hold one
    /// task per chunk.
    async fn collect_join(
        handle: JoinHandle<Result<(), String>>,
        storage_module_id: usize,
        failed: &mut Option<String>,
    ) {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(
                    target: "irys::packing",
                    "CPU packing task failed for SM {}: {}", storage_module_id, err
                );
                failed.get_or_insert(err);
            }
            Err(err) => {
                warn!(
                    target: "irys::packing",
                    "CPU packing task failed for SM {}: {:?}", storage_module_id, err
                );
                failed.get_or_insert(format!(
                    "CPU packing task failed for SM {storage_module_id}: {err:?}"
                ));
            }
        }
    }
}

#[async_trait]
impl super::PackingStrategy for CpuPackingStrategy {
    #[tracing::instrument(level = "trace", skip_all, fields(storage_module.id = storage_module_id, chunk.range_start = *chunk_range.0.start(), chunk.range_end = *chunk_range.0.end(), partition.hash = %partition_hash))]
    async fn pack(
        &self,
        storage_module: &Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
        mining_address: IrysAddress,
        partition_hash: PartitionHash,
        storage_module_id: usize,
        short_writes_before_sync: u32,
    ) -> Result<(), String> {
        let range_start = *chunk_range.0.start();

        #[cfg(test)]
        {
            let pause = self.pack_pause.lock().expect("pack pause").clone();
            if let Some(pause) = pause {
                pause.started.notify_one();
                pause.release.notified().await;
                if let Some(reason) = pause.fail_with.lock().expect("pack pause fail").clone() {
                    return Err(reason);
                }
            }
        }

        let mut handles = VecDeque::new();
        let join_cap = usize::from(self.concurrency).max(1);
        let mut failed = None;
        #[cfg(test)]
        let write_gate = self.write_gate.lock().expect("write gate").clone();

        // Hint only. Empty means nothing in this range is uninitialized, or
        // the hash is no longer assigned. The write is the guard for a label
        // that changes after this read.
        let gaps = storage_module.entropy_gaps(partition_hash, chunk_range.0);
        if gaps.is_empty() {
            let still_assigned = storage_module
                .partition_assignment()
                .is_some_and(|assignment| assignment.partition_hash == partition_hash);
            if !still_assigned {
                return Err(format!(
                    "entropy write refused at offset {range_start} for SM {storage_module_id}"
                ));
            }
            return Ok(());
        }

        'spans: for span in gaps {
            for i in span.start().0..=span.end().0 {
                #[cfg(test)]
                {
                    self.chunks_started
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                // Wait for the disk before queueing more entropy.
                if i % short_writes_before_sync == 0 {
                    if let Err(err) = crate::packing_service::wait_for_pending_room(
                        storage_module,
                        "CPU packing (periodic)",
                    )
                    .await
                    {
                        failed = Some(err);
                        break 'spans;
                    }
                    yield_now().await;
                }

                // Acquire permit for this chunk
                let permit = self
                    .semaphore
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("Failure acquiring a CPU packing semaphore");

                let chunk_size = self.chunk_size;
                let entropy_iterations = self.entropy_iterations;
                let storage_module_clone = storage_module.clone();
                let chain_id = self.chain_id;
                let notify = self.notify.clone();
                #[cfg(test)]
                let write_gate = write_gate.clone();

                // Pack in blocking thread pool
                let handle = self.runtime_handle
                .clone()
                .spawn_blocking(move || {
                    let mut out = Vec::with_capacity(chunk_size);
                    compute_entropy_chunk(
                        mining_address,
                        i as u64,
                        partition_hash.0,
                        entropy_iterations,
                        chunk_size,
                        &mut out,
                        chain_id,
                    );

                    debug!(
                        target: "irys::packing::progress",
                        "CPU Packing chunk offset {} for SM {} partition_hash {} mining_address {} iterations {}",
                        &i,
                        &storage_module_id,
                        &partition_hash,
                        &mining_address,
                        entropy_iterations
                    );

                    // The label can change while this chunk is hashed. Data
                    // that landed in that window is not overwritten.
                    #[cfg(test)]
                    if let Some(gate) = write_gate {
                        gate.wait_turn();
                    }

                    let wrote = storage_module_clone.write_entropy_chunk(
                        PartitionChunkOffset::from(i),
                        out,
                        partition_hash,
                    );
                    drop(permit);
                    notify.notify_waiters();
                    match wrote {
                        EntropyWrite::Queued
                        | EntropyWrite::Rejected(EntropyRejection::NotUninitialized(_)) => Ok(()),
                        EntropyWrite::Rejected(EntropyRejection::NotAssigned) => Err(format!(
                            "entropy write refused at offset {i} for SM {storage_module_id}"
                        )),
                    }
                });

                handles.push_back(handle);
                #[cfg(test)]
                self.note_retained(handles.len());
                if handles.len() >= join_cap
                    && let Some(oldest) = handles.pop_front()
                {
                    Self::collect_join(oldest, storage_module_id, &mut failed).await;
                    if failed.is_some() {
                        // A refused write fails the whole range. Do not hash the rest.
                        break 'spans;
                    }
                }

                // Log progress periodically
                crate::packing_service::log_packing_progress(
                    "CPU",
                    i,
                    &chunk_range,
                    storage_module_id,
                    &partition_hash,
                    &mining_address,
                );
            }
        }

        // A refused write or a panicked worker must fail the range so the
        // caller can requeue it. Handles past `join_cap` were already joined.
        while let Some(handle) = handles.pop_front() {
            Self::collect_join(handle, storage_module_id, &mut failed).await;
        }
        if let Some(err) = failed {
            return Err(err);
        }

        trace!(
            target: "irys::packing::done",
            "CPU Packed chunk {} - {} for SM {} partition_hash {} mining_address {:?} iterations {}",
            chunk_range.0.start(),
            chunk_range.0.end(),
            &storage_module_id,
            &partition_hash,
            &mining_address,
            self.entropy_iterations
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, NodeConfig, PartitionChunkOffset, PartitionChunkRange, ii,
        partition::PartitionHash, partition_chunk_offset_ii,
    };
    use tokio::sync::{Notify, Semaphore};

    use super::super::PackingStrategy as _;
    use super::CpuPackingStrategy;

    fn test_module(
        prefix: &str,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Arc<irys_types::Config>,
        Arc<StorageModule>,
        PartitionHash,
    ) {
        test_module_span(prefix, 4)
    }

    fn test_module_span(
        prefix: &str,
        chunks: u64,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Arc<irys_types::Config>,
        Arc<StorageModule>,
        PartitionHash,
    ) {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: chunks,
            chunk_size: 32,
            ..consensus
        });
        let config = Arc::new(irys_types::Config::new_with_random_peer_id(node_config));
        let partition_hash = PartitionHash::repeat_byte(0x3C);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(irys_types::partition::PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(
                irys_types::partition_chunk_offset_ie!(0, chunks as u32),
                "hdd0".into(),
            )],
        };
        let module = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        (tmp_dir, config, module, partition_hash)
    }

    fn strategy(config: Arc<irys_types::Config>) -> CpuPackingStrategy {
        const CONCURRENCY: u16 = 2;
        CpuPackingStrategy::new(
            config.clone(),
            Arc::new(Semaphore::new(usize::from(CONCURRENCY))),
            Arc::new(Notify::new()),
            tokio::runtime::Handle::current(),
            config.consensus.chain_id,
            CONCURRENCY,
            Arc::new(std::sync::Mutex::new(None)),
        )
    }

    /// A refused write fails the range. The loop must not hash every later chunk.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_stops_after_a_refused_write() {
        const CHUNKS: u32 = 16;
        let (_tmp, config, module, _hash) = test_module_span("cpu_stop_refused", u64::from(CHUNKS));
        let packer = strategy(config.clone());
        let result = packer
            .pack(
                &module,
                PartitionChunkRange(partition_chunk_offset_ii!(0, CHUNKS - 1)),
                config.node_config.miner_address(),
                PartitionHash::repeat_byte(0x11),
                module.id,
                100,
            )
            .await;
        assert!(
            result.is_err(),
            "a refused write must fail the pack, got {result:?}"
        );
        let started = packer
            .chunks_started
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            started, 0,
            "a range the module will not accept is not hashed, started {started}"
        );
    }

    /// A pack for another partition hash fails and stores nothing. A pack for
    /// the assigned hash writes entropy.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_follows_the_assigned_hash() {
        let cases = [
            (
                false,
                "a refused entropy write must fail the pack",
                ChunkType::Uninitialized,
            ),
            (
                true,
                "packing for the assigned hash must succeed",
                ChunkType::Entropy,
            ),
        ];
        for (own_hash, message, kind) in cases {
            let (_tmp, config, module, hash) = test_module("cpu_assigned_hash");
            let hash = if own_hash {
                hash
            } else {
                PartitionHash::repeat_byte(0x11)
            };
            let result = strategy(config.clone())
                .pack(
                    &module,
                    PartitionChunkRange(partition_chunk_offset_ii!(0, 0)),
                    config.node_config.miner_address(),
                    hash,
                    module.id,
                    100,
                )
                .await;
            assert_eq!(result.is_ok(), own_hash, "{message}, got {result:?}");
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset(0)),
                Some(kind),
                "{message}"
            );
        }
    }

    /// A retried range can contain Data that landed after an earlier entropy
    /// prefix. Those offsets stay Data. Uninitialized offsets are still packed.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_does_not_write_entropy_over_data() {
        let (_tmp, config, module, hash) = test_module_span("cpu_keep_data", 4);
        for offset in [0_u32, 2] {
            assert!(module.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![0xD4_u8; 32],
                ChunkType::Data,
            ));
        }
        module.force_sync_pending_chunks().expect("data is durable");
        let result = strategy(config.clone())
            .pack(
                &module,
                PartitionChunkRange(partition_chunk_offset_ii!(0, 3)),
                config.node_config.miner_address(),
                hash,
                module.id,
                100,
            )
            .await;
        assert!(
            result.is_ok(),
            "data in the range must not fail the pack, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Data),
            "entropy must not replace data at the start of a retried range"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(2)),
            Some(ChunkType::Data),
            "entropy must not replace data in the middle of a retried range"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Entropy),
            "an uninitialized offset in the same range is still packed"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(3)),
            Some(ChunkType::Entropy),
            "the uninitialized tail is still packed"
        );
    }

    /// Entropy is already the pack target. The range still packs the holes
    /// and does not fail because a prefix is packed.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_skips_entropy_and_packs_the_uninitialized_tail() {
        let (_tmp, config, module, hash) = test_module_span("cpu_skip_entropy", 2);
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0xE1_u8; 32],
            ChunkType::Entropy,
        ));
        module
            .force_sync_pending_chunks()
            .expect("entropy is durable");
        let result = strategy(config.clone())
            .pack(
                &module,
                PartitionChunkRange(partition_chunk_offset_ii!(0, 1)),
                config.node_config.miner_address(),
                hash,
                module.id,
                100,
            )
            .await;
        assert!(
            result.is_ok(),
            "an entropy prefix must not fail the pack, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy)
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(1)),
            Some(ChunkType::Entropy),
            "the uninitialized tail is still packed"
        );
    }

    /// Data queued after the chunk is hashed must stay data. The write is the
    /// guard. The gate is the window between the hash and that write.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_does_not_overwrite_data_queued_while_the_chunk_is_hashed() {
        let (_tmp, config, module, hash) = test_module_span("cpu_mid_pack_data", 2);
        let packer = strategy(config.clone());
        let gate = Arc::new(super::WriteGate::new());
        packer.set_write_gate(Some(Arc::clone(&gate)));
        let module_for_pack = Arc::clone(&module);
        let miner = config.node_config.miner_address();
        let sm_id = module.id;
        let packed = tokio::spawn(async move {
            packer
                .pack(
                    &module_for_pack,
                    PartitionChunkRange(partition_chunk_offset_ii!(0, 0)),
                    miner,
                    hash,
                    sm_id,
                    100,
                )
                .await
        });
        // The packer waits inside `spawn_blocking`. Waiting here on the
        // runtime thread would leave that task unpolled.
        let gate_for_wait = Arc::clone(&gate);
        let reached = tokio::task::spawn_blocking(move || {
            gate_for_wait.wait_arrived(std::time::Duration::from_secs(5))
        })
        .await
        .expect("gate wait");
        assert!(reached, "pack did not reach the entropy write");
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0xD4_u8; 32],
            ChunkType::Data,
        ));
        gate.release();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), packed)
            .await
            .expect("pack did not finish")
            .expect("pack task");
        assert!(
            result.is_ok(),
            "queued data must not fail the pack, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Data),
            "entropy replaced the data body queued while the chunk was hashed"
        );
    }

    /// A long range used to push one join handle per chunk and await them only
    /// at the end. Finished `spawn_blocking` tasks stay allocated until then.
    /// A 75,534,400-chunk partition at a few hundred bytes per task is
    /// multi-gigabyte resident set before the range returns.
    #[test_log::test(tokio::test)]
    async fn cpu_pack_joins_tasks_as_the_range_advances() {
        const CHUNKS: u32 = 16;
        let (_tmp, config, module, hash) = test_module_span("cpu_bounded_joins", u64::from(CHUNKS));
        let packer = strategy(config.clone());
        let result = packer
            .pack(
                &module,
                PartitionChunkRange(partition_chunk_offset_ii!(0, CHUNKS - 1)),
                config.node_config.miner_address(),
                hash,
                module.id,
                100,
            )
            .await;
        assert!(
            result.is_ok(),
            "bounded pack must still write the range, got {result:?}"
        );
        let peak = packer
            .peak_retained_joins
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            peak <= usize::from(packer.concurrency),
            "retained {peak} join handles; cap is {}",
            packer.concurrency
        );
        assert!(
            peak >= usize::from(packer.concurrency),
            "a range longer than the cap must fill it, peak was {peak}"
        );
        for offset in 0..CHUNKS {
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset(offset)),
                Some(ChunkType::Entropy),
                "offset {offset}"
            );
        }
    }

    /// Lower bound on the bytes a finished `spawn_blocking` task holds until
    /// its join handle is awaited. The production loop used to keep one of
    /// these per chunk. Linux only: the check reads `VmRSS`.
    #[cfg(target_os = "linux")]
    #[test_log::test(tokio::test)]
    async fn completed_spawn_blocking_tasks_stay_resident_until_joined() {
        const TASKS: usize = 20_000;
        let before = vm_rss_kb();
        let semaphore = Arc::new(Semaphore::new(2));
        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore open");
            handles.push(tokio::task::spawn_blocking(move || {
                drop(permit);
            }));
        }
        let during = vm_rss_kb();
        for handle in handles {
            handle.await.expect("blocking task");
        }
        let after = vm_rss_kb();
        let growth_kb = during.saturating_sub(before);
        let per_task = growth_kb.saturating_mul(1024) / TASKS as u64;
        eprintln!(
            "retained spawn_blocking rss: before={before}KiB during={during}KiB after={after}KiB tasks={TASKS} per_task≈{per_task}B"
        );
        // A `Vec` of join handles is tens of bytes per task. Retained
        // `spawn_blocking` stacks leave a few hundred bytes each. A floor
        // under that residue fails when the runtime frees the stacks.
        assert!(
            per_task >= 256,
            "retaining {TASKS} completed tasks grew {per_task} B each ({before} -> {during} KiB); expected at least 256 B of stack residue per task"
        );
    }

    #[cfg(target_os = "linux")]
    fn vm_rss_kb() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").expect("status");
        for line in status.lines() {
            let Some(rest) = line.strip_prefix("VmRSS:") else {
                continue;
            };
            return rest
                .split_whitespace()
                .next()
                .expect("rss")
                .parse()
                .expect("rss number");
        }
        panic!("VmRSS missing");
    }
}
