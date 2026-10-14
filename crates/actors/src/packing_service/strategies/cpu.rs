use std::sync::Arc;

use async_trait::async_trait;
use irys_domain::StorageModule;
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_types::IrysAddress;
use irys_types::{Config, PartitionChunkOffset, PartitionChunkRange, partition::PartitionHash};
use tokio::sync::{Notify, Semaphore};
use tokio::task::yield_now;
use tracing::{debug, trace, warn};

/// CPU-based packing strategy
pub(crate) struct CpuPackingStrategy {
    chunk_size: usize,
    entropy_iterations: u32,
    semaphore: Arc<Semaphore>,
    notify: Arc<Notify>,
    runtime_handle: tokio::runtime::Handle,
    chain_id: u64,
}

impl CpuPackingStrategy {
    pub(crate) fn new(
        config: Arc<Config>,
        semaphore: Arc<Semaphore>,
        notify: Arc<Notify>,
        runtime_handle: tokio::runtime::Handle,
        chain_id: u64,
    ) -> Self {
        Self {
            chunk_size: config.consensus.chunk_size as usize,
            entropy_iterations: config.consensus.entropy_packing_iterations,
            semaphore,
            notify,
            runtime_handle,
            chain_id,
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

        let mut handles = Vec::new();

        for i in range_start..=range_end {
            // Periodic sync to avoid memory buildup
            if i % short_writes_before_sync == 0 {
                crate::packing_service::sync_with_warning(storage_module, "CPU packing (periodic)");
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

            handles.push(handle);

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

        // Wait for all packing tasks to complete. A refused write or a
        // panicked worker must fail the range so the caller can requeue it.
        let mut failed = None;
        for handle in handles {
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
        let tmp_dir = TempDirBuilder::new().prefix(prefix).build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp_dir.path().to_path_buf();
        let consensus = node_config.consensus_config();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            entropy_packing_iterations: 1,
            num_chunks_in_partition: 4,
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
            submodules: vec![(irys_types::partition_chunk_offset_ie!(0, 4), "hdd0".into())],
        };
        let module = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        (tmp_dir, config, module, partition_hash)
    }

    fn strategy(config: Arc<irys_types::Config>) -> CpuPackingStrategy {
        CpuPackingStrategy::new(
            config.clone(),
            Arc::new(Semaphore::new(2)),
            Arc::new(Notify::new()),
            tokio::runtime::Handle::current(),
            config.consensus.chain_id,
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
}
