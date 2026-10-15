use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use irys_domain::StorageModule;
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_types::IrysAddress;
use irys_types::{Config, PartitionChunkOffset, PartitionChunkRange, partition::PartitionHash};
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, yield_now};
use tracing::{debug, trace, warn};

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
}

impl CpuPackingStrategy {
    pub(crate) fn new(
        config: Arc<Config>,
        semaphore: Arc<Semaphore>,
        notify: Arc<Notify>,
        runtime_handle: tokio::runtime::Handle,
        chain_id: u64,
        concurrency: u16,
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
        }
    }

    #[cfg(test)]
    fn note_retained(&self, len: usize) {
        self.peak_retained_joins.fetch_max(len, Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn note_retained(&self, _len: usize) {}

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
        let range_end = *chunk_range.0.end();

        let mut handles = VecDeque::new();
        let join_cap = usize::from(self.concurrency).max(1);
        let mut failed = None;

        for i in range_start..=range_end {
            // Wait for the disk before queueing more entropy.
            if i % short_writes_before_sync == 0 {
                if let Err(err) = crate::packing_service::wait_for_pending_room(
                    storage_module,
                    "CPU packing (periodic)",
                )
                .await
                {
                    failed = Some(err);
                    break;
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

                    let wrote = storage_module_clone.write_entropy_chunk(
                        PartitionChunkOffset::from(i),
                        out,
                        partition_hash,
                    );
                    drop(permit);
                    notify.notify_waiters();
                    if !wrote {
                        return Err(format!(
                            "entropy write refused at offset {i} for SM {storage_module_id}"
                        ));
                    }
                    Ok(())
                });

            handles.push_back(handle);
            self.note_retained(handles.len());
            if handles.len() >= join_cap
                && let Some(oldest) = handles.pop_front()
            {
                Self::collect_join(oldest, storage_module_id, &mut failed).await;
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
        )
    }

    #[test_log::test(tokio::test)]
    async fn refused_entropy_write_fails_cpu_pack() {
        let (_tmp, config, module, _assigned) = test_module("cpu_refused_write");
        let other_hash = PartitionHash::repeat_byte(0x11);
        let result = strategy(config.clone())
            .pack(
                &module,
                PartitionChunkRange(partition_chunk_offset_ii!(0, 0)),
                config.node_config.miner_address(),
                other_hash,
                module.id,
                100,
            )
            .await;
        assert!(
            result.is_err(),
            "a refused entropy write must fail the pack, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Uninitialized)
        );
    }

    #[test_log::test(tokio::test)]
    async fn cpu_pack_writes_entropy_for_the_assigned_hash() {
        let (_tmp, config, module, hash) = test_module("cpu_assigned_write");
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
        assert!(
            result.is_ok(),
            "packing for the assigned hash must succeed, got {result:?}"
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset(0)),
            Some(ChunkType::Entropy)
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
        assert!(
            growth_kb > 0,
            "retaining {TASKS} completed tasks did not grow VmRSS ({before} -> {during})"
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
