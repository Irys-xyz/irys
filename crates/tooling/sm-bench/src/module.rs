//! Open one real `StorageModule` on the bench directory.
//!
//! The submodule file is not `chunks.dat`. Recall reads use the entropy file.
//! Writes, entropy-for-XOR preads, and Data reads use the write file. Both
//! descriptors hang off the one disk lane.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use eyre::{WrapErr as _, eyre};
use irys_domain::{
    BenchIndex, SplitBacking, StorageModule, StorageModuleInfo, write_entropy_intervals,
};
use irys_types::{
    Config, ConsensusConfig, ConsensusOptions, DataLedger, H256, NodeConfig, PartitionChunkOffset,
    StorageSyncConfig, ii, partition::PartitionAssignment, partition_chunk_offset_ii,
};

use crate::guard::CHUNK_SIZE;

pub(crate) struct OpenedModule {
    pub module: Arc<StorageModule>,
    pub index: BenchIndex,
    lane: Option<JoinHandle<()>>,
}

pub(crate) struct OpenRequest<'a> {
    pub dir: &'a Path,
    pub recall_file: &'a Path,
    pub write_file: &'a Path,
    pub partition_chunks: u32,
    pub recall_chunks: u64,
    pub write_origin: u32,
    pub write_chunks: u64,
    pub num_writes_before_sync: u64,
    pub entropy_sweep_max_bytes: u64,
    pub intervals_persist_secs: u64,
    pub index: BenchIndex,
    /// Registrations per fsync. `0` skips the call; the method rejects 0.
    pub index_group_commit: u32,
}

pub(crate) fn open_module(request: OpenRequest<'_>) -> eyre::Result<OpenedModule> {
    let dir = request.dir;
    std::fs::create_dir_all(dir)?;
    let chunks_dat = dir.join("chunks.dat");
    if chunks_dat.exists() {
        eprintln!(
            "irys-sm-bench: {} exists and stays closed; the bench does not use it",
            chunks_dat.display()
        );
    }
    write_entropy_intervals(&dir.join("intervals.json"), request.partition_chunks)?;

    let parent = dir
        .parent()
        .ok_or_else(|| eyre!("bench dir {} has no parent", dir.display()))?;
    let name = dir
        .file_name()
        .ok_or_else(|| eyre!("bench dir {} has no file name", dir.display()))?;

    let mut node = NodeConfig::testing();
    node.base_directory = parent.to_path_buf();
    node.consensus = ConsensusOptions::Custom(ConsensusConfig {
        chunk_size: CHUNK_SIZE,
        num_chunks_in_partition: u64::from(request.partition_chunks),
        num_chunks_in_recall_range: request.recall_chunks,
        entropy_packing_iterations: 10,
        ..ConsensusConfig::testing()
    });
    node.storage = StorageSyncConfig {
        num_writes_before_sync: request.num_writes_before_sync,
        entropy_sweep_max_bytes: request.entropy_sweep_max_bytes,
        intervals_persist_secs: request.intervals_persist_secs,
        ..StorageSyncConfig::default()
    };
    // Production durability and the 2 TiB submodule geometry. The testing
    // config uses UtterlyNoSync and a 4 GiB cap, which would not contend the
    // way a node index commit does.
    node.database = irys_types::DatabaseConfig::default();
    let config = Config::new_with_random_peer_id(node);
    let info = StorageModuleInfo {
        id: 0,
        partition_assignment: Some(PartitionAssignment {
            partition_hash: H256::from([0xB7; 32]),
            miner_address: config.node_config.miner_address(),
            ledger_id: Some(u32::from(DataLedger::Publish)),
            slot_index: Some(0),
        }),
        submodules: vec![(
            partition_chunk_offset_ii!(0, request.partition_chunks - 1),
            PathBuf::from(name),
        )],
    };

    let recall = OpenOptions::new()
        .read(true)
        .open(request.recall_file)
        .wrap_err_with(|| {
            format!(
                "failed to open recall file {}",
                request.recall_file.display()
            )
        })?;
    let write = OpenOptions::new()
        .read(true)
        .write(true)
        .open(request.write_file)
        .wrap_err_with(|| format!("failed to open write file {}", request.write_file.display()))?;
    let split = SplitBacking::open(recall, write, request.write_origin, request.write_chunks)?;
    let module = Arc::new(StorageModule::open_split(
        &info,
        &config,
        Arc::new(split),
        request.index,
    )?);
    if request.index_group_commit > 0 {
        module.enable_index_group_commit(request.index_group_commit)?;
    }
    Ok(OpenedModule {
        module,
        index: request.index,
        lane: None,
    })
}

impl OpenedModule {
    /// Start the one disk-lane thread. Call this after index prepopulate so
    /// that bulk put does not run beside the lane.
    pub(crate) fn start_lane(&mut self) -> eyre::Result<()> {
        if self.lane.is_some() {
            return Ok(());
        }
        let lane = self
            .module
            .spawn_disk_lane()
            .wrap_err("failed to spawn the disk lane")?
            .ok_or_else(|| eyre!("disk lane was already running"))?;
        self.lane = Some(lane);
        Ok(())
    }

    pub(crate) fn stop_lane(&mut self) -> eyre::Result<()> {
        self.module.stop_disk_lane();
        if let Some(handle) = self.lane.take() {
            handle
                .join()
                .map_err(|_| eyre!("disk lane thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for OpenedModule {
    fn drop(&mut self) {
        self.module.stop_disk_lane();
        if let Some(handle) = self.lane.take() {
            let _ = handle.join();
        }
    }
}

/// Copied from the body-migration worker. That helper is crate-private.
pub(crate) fn pending_ceiling(num_writes_before_sync: u64, chunk_size: u64) -> u64 {
    let one_batch = num_writes_before_sync.saturating_mul(chunk_size);
    let derived = (num_writes_before_sync.saturating_mul(2))
        .max(256)
        .saturating_mul(chunk_size);
    derived.max(one_batch)
}

pub(crate) fn flush_batch(num_writes_before_sync: u64, chunk_size: u64) -> u64 {
    num_writes_before_sync.saturating_mul(chunk_size)
}

pub(crate) fn span_limit(entropy_sweep_max_bytes: u64, chunk_size: u64) -> usize {
    let span = (entropy_sweep_max_bytes / chunk_size.max(1)).clamp(1, 512);
    usize::try_from(span).unwrap_or(512)
}
