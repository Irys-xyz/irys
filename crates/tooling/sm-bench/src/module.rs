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
    BenchIndex, SplitBacking, StorageModule, StorageModuleInfo, read_intervals_file,
    write_entropy_intervals,
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
    pub entropy_read_grace_ms: u64,
    pub entropy_read_batch_bytes: u64,
    pub entropy_readahead_bytes: u64,
    pub entropy_readahead_cache_bytes: u64,
    pub intervals_persist_secs: u64,
    /// `0` runs a durable point on every gap. The bench default is the node
    /// default, 250, for HDD storage modules.
    pub min_durable_interval_ms: u64,
    /// Parts per thousand of the pending-byte ceiling. See
    /// `StorageSyncConfig::durable_headroom_permille`.
    pub durable_headroom_permille: u64,
    /// Mining recall uses an `O_DIRECT` descriptor on `recall_file`.
    pub recall_direct_io: bool,
    /// File chunk where partition chunk 0 starts. `0` reads the start of the file.
    pub recall_offset_chunks: u32,
    pub index: BenchIndex,
    /// Arms group commit. MDBX syncs every N. RocksDB does not.
    /// `0` skips the call; the method rejects 0.
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
    let intervals_path = dir.join("intervals.json");
    // An existing file is normalized before open. A span that is only
    // Uninitialized or Interrupted becomes Entropy, and open loads that span.
    // Replacing it after open would let open reconcile those chunks as
    // Uninitialized. A missing file is written only after open returns: a
    // failure before the index exists must not leave a new intervals.json.
    let intervals_missing = !intervals_path.exists();
    if !intervals_missing {
        write_entropy_intervals(&intervals_path, request.partition_chunks)?;
    }

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
        entropy_read_grace_ms: request.entropy_read_grace_ms,
        entropy_read_batch_bytes: request.entropy_read_batch_bytes,
        entropy_readahead_bytes: request.entropy_readahead_bytes,
        entropy_readahead_cache_bytes: request.entropy_readahead_cache_bytes,
        intervals_persist_secs: request.intervals_persist_secs,
        min_durable_interval_ms: request.min_durable_interval_ms,
        durable_headroom_permille: request.durable_headroom_permille,
        recall_direct_io: request.recall_direct_io,
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
    let mut split = SplitBacking::open(recall, write, request.write_origin, request.write_chunks)?
        .with_recall_origin(request.recall_offset_chunks);
    // Mining recall only. The buffered recall descriptor above still serves
    // non-data reads. `Ok(None)` keeps those recalls buffered.
    if request.recall_direct_io
        && let Some(direct) = irys_domain::open_recall_direct(request.recall_file)?
    {
        split = split.with_direct_recall(direct)?;
    }
    let module = Arc::new(StorageModule::open_split(
        &info,
        &config,
        Arc::new(split),
        request.index,
    )?);
    if intervals_missing {
        write_entropy_intervals(&intervals_path, request.partition_chunks)?;
        // Open stored the default Uninitialized span. The file is now Entropy.
        // The live map must match, or the next rewrite puts Uninitialized back.
        let loaded = read_intervals_file(&intervals_path)?;
        let mut live = module
            .intervals()
            .write()
            .map_err(|_| eyre!("intervals lock poisoned"))?;
        *live = loaded;
    }
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

pub(crate) fn flush_batch(num_writes_before_sync: u64, chunk_size: u64) -> u64 {
    num_writes_before_sync.saturating_mul(chunk_size)
}

pub(crate) fn span_limit(entropy_sweep_max_bytes: u64, chunk_size: u64) -> usize {
    let span = (entropy_sweep_max_bytes / chunk_size.max(1)).clamp(1, 512);
    usize::try_from(span).unwrap_or(512)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use irys_domain::{BenchIndex, ChunkType, StorageModule};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{PartitionChunkOffset, ii, partition_chunk_offset_ii};

    use super::{OpenRequest, open_module};
    use crate::guard::CHUNK_SIZE;

    /// File chunk `i` is filled with `0xA0 + i`.
    fn write_marked(path: &Path, chunks: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        for index in 0..chunks {
            let mark = 0xA0 + u8::try_from(index).unwrap();
            file.write_all(&vec![mark; CHUNK_SIZE as usize]).unwrap();
        }
        file.sync_all().unwrap();
    }

    fn write_uniform(path: &Path, chunks: u64, mark: u8) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        for _ in 0..chunks {
            file.write_all(&vec![mark; CHUNK_SIZE as usize]).unwrap();
        }
        file.sync_all().unwrap();
    }

    fn open_slice(
        dir: &Path,
        recall: &Path,
        write: &Path,
        recall_offset_chunks: u32,
        direct: bool,
    ) -> super::OpenedModule {
        fs::create_dir_all(dir).unwrap();
        open_module(OpenRequest {
            dir,
            recall_file: recall,
            write_file: write,
            partition_chunks: 2,
            recall_chunks: 1,
            write_origin: 0,
            write_chunks: 1,
            num_writes_before_sync: 100,
            entropy_sweep_max_bytes: CHUNK_SIZE,
            entropy_read_grace_ms: 0,
            entropy_read_batch_bytes: CHUNK_SIZE,
            entropy_readahead_bytes: 0,
            entropy_readahead_cache_bytes: 0,
            intervals_persist_secs: 0,
            min_durable_interval_ms: 0,
            durable_headroom_permille: 1000,
            recall_direct_io: direct,
            recall_offset_chunks,
            // MDBX-SUBMODULE-INDEX: retarget this open at BenchIndex::Rocks. The check is not MDBX-specific.
            index: BenchIndex::Mdbx,
            index_group_commit: 0,
        })
        .unwrap_or_else(|error| panic!("open {}: {error}", dir.display()))
    }

    fn uniform_mark(bytes: &[u8], kind: ChunkType, offset: u32) -> u8 {
        assert_eq!(kind, ChunkType::Entropy, "offset {offset}");
        assert_eq!(bytes.len(), CHUNK_SIZE as usize, "offset {offset}");
        let mark = bytes[0];
        assert!(
            bytes.iter().all(|byte| *byte == mark),
            "offset {offset} is not one mark"
        );
        mark
    }

    fn mining_mark(module: &StorageModule, offset: u32) -> u8 {
        let range = partition_chunk_offset_ii!(offset, offset);
        let got = module
            .read_recall_chunks(range)
            .unwrap_or_else(|error| panic!("recall {offset}: {error}"));
        let (bytes, kind) = got
            .chunks
            .get(&PartitionChunkOffset::from(offset))
            .unwrap_or_else(|| panic!("recall missed partition chunk {offset}"));
        uniform_mark(bytes, *kind, offset)
    }

    fn serve_mark(module: &StorageModule, offset: u32) -> u8 {
        let range = partition_chunk_offset_ii!(offset, offset);
        let got = module
            .read_chunks(range)
            .unwrap_or_else(|error| panic!("read {offset}: {error}"));
        let (bytes, kind) = got
            .get(&PartitionChunkOffset::from(offset))
            .unwrap_or_else(|| panic!("read missed partition chunk {offset}"));
        uniform_mark(bytes, *kind, offset)
    }

    #[test]
    fn recall_offset_reads_file_chunk_n_plus_the_partition_offset() {
        // Breaks if partition chunk 0 is read at file offset 0 when N is 2.
        let tmp = TempDirBuilder::new().prefix("sm-recall-offset-").build();
        let root = tmp.path().join("loadtest");
        let recall = root.join("recall.dat");
        write_marked(&recall, 4);
        let write = root.join("write.dat");
        write_uniform(&write, 1, 0x11);

        for direct in [false, true] {
            let dir = root.join(if direct { "direct" } else { "buffered" });
            let opened = open_slice(&dir, &recall, &write, 2, direct);
            let mode = opened.module.recall_io_mode();
            if direct {
                assert_eq!(mode, "direct", "O_DIRECT recall did not open");
            } else {
                assert_eq!(mode, "buffered");
            }
            assert_eq!(
                mining_mark(&opened.module, 0),
                0xA2,
                "direct={direct} mode={mode} partition chunk 0"
            );
            assert_eq!(
                mining_mark(&opened.module, 1),
                0xA3,
                "direct={direct} mode={mode} partition chunk 1"
            );
            assert_eq!(
                serve_mark(&opened.module, 0),
                0xA2,
                "direct={direct} mode={mode} non-recall read"
            );
            drop(opened);
        }
    }

    #[test]
    fn two_benches_open_the_same_recall_file() {
        // Breaks if the second open of a read-only recall file fails or blocks.
        let tmp = TempDirBuilder::new().prefix("sm-recall-shared-").build();
        let root = tmp.path().join("loadtest");
        let recall = root.join("recall.dat");
        write_marked(&recall, 4);
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();
        for (name, tx) in [("a", tx_a), ("b", tx_b)] {
            let recall = recall.clone();
            let root = root.clone();
            let name = name.to_string();
            thread::spawn(move || {
                let dir = root.join(&name);
                let write = root.join(format!("{name}-write.dat"));
                write_uniform(&write, 1, 0x11);
                let opened = open_slice(&dir, &recall, &write, 2, false);
                let mark = mining_mark(&opened.module, 0);
                let _ = tx.send(mark);
            });
        }
        let wait = Duration::from_secs(90);
        let mark_a = rx_a
            .recv_timeout(wait)
            .expect("first bench did not open the recall file within 90s");
        let mark_b = rx_b
            .recv_timeout(wait)
            .expect("second bench did not open the recall file within 90s");
        assert_eq!(mark_a, 0xA2);
        assert_eq!(mark_b, 0xA2);
    }

    fn open_request<'a>(dir: &'a Path, recall: &'a Path, write: &'a Path) -> OpenRequest<'a> {
        OpenRequest {
            dir,
            recall_file: recall,
            write_file: write,
            partition_chunks: 2,
            recall_chunks: 1,
            write_origin: 0,
            write_chunks: 1,
            num_writes_before_sync: 100,
            entropy_sweep_max_bytes: CHUNK_SIZE,
            entropy_read_grace_ms: 0,
            entropy_read_batch_bytes: CHUNK_SIZE,
            entropy_readahead_bytes: 0,
            entropy_readahead_cache_bytes: 0,
            intervals_persist_secs: 0,
            min_durable_interval_ms: 0,
            durable_headroom_permille: 1000,
            recall_direct_io: false,
            recall_offset_chunks: 0,
            // MDBX-SUBMODULE-INDEX: retarget this open at BenchIndex::Rocks. The check is not MDBX-specific.
            index: BenchIndex::Mdbx,
            index_group_commit: 0,
        }
    }

    #[test]
    fn a_failed_write_open_does_not_create_intervals() {
        // Breaks if intervals.json is written before the write file opens.
        // The index is not created on this path, so a later restore must be
        // able to put the previous intervals file back.
        let tmp = TempDirBuilder::new().prefix("sm-intervals-fail-").build();
        let root = tmp.path().join("loadtest");
        fs::create_dir_all(&root).unwrap();
        let recall = root.join("recall.dat");
        write_uniform(&recall, 2, 0xA2);
        let write = root.join("write.dat");
        write_uniform(&write, 1, 0x11);
        let mut perms = fs::metadata(&write).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&write, perms).unwrap();
        let dir = root.join("mod");
        let error = match open_module(open_request(&dir, &recall, &write)) {
            Ok(_) => panic!("a read-only write file must not open"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("write file"),
            "unexpected open error: {message}"
        );
        assert!(
            !dir.join("intervals.json").exists(),
            "intervals.json was created before the write file opened"
        );
        assert!(
            !dir.join("db").exists(),
            "index db exists after a failed open"
        );
        assert!(
            !dir.join("rocks").exists(),
            "index rocks exists after a failed open"
        );
    }

    #[test]
    fn a_missing_intervals_file_is_entropy_after_a_successful_open() {
        // Breaks if a successful open leaves Uninitialized labels, or if the
        // live map does not match the file the open published.
        let tmp = TempDirBuilder::new().prefix("sm-intervals-ok-").build();
        let root = tmp.path().join("loadtest");
        let recall = root.join("recall.dat");
        write_uniform(&recall, 2, 0xA2);
        let write = root.join("write.dat");
        write_uniform(&write, 1, 0x11);
        let dir = root.join("mod");
        let opened = open_module(open_request(&dir, &recall, &write))
            .unwrap_or_else(|error| panic!("open: {error:#}"));
        let text = fs::read_to_string(dir.join("intervals.json")).unwrap();
        assert!(text.contains("\"Entropy\""), "{text}");
        assert!(!text.contains("Uninitialized"), "{text}");
        assert_eq!(mining_mark(&opened.module, 0), 0xA2);
        assert!(
            opened
                .module
                .get_intervals(ChunkType::Uninitialized)
                .is_empty()
        );
        assert!(!opened.module.get_intervals(ChunkType::Entropy).is_empty());
    }

    #[test]
    fn an_existing_data_intervals_file_is_kept() {
        // Breaks if open replaces a Data span with Entropy.
        let tmp = TempDirBuilder::new().prefix("sm-intervals-data-").build();
        let root = tmp.path().join("loadtest");
        fs::create_dir_all(root.join("mod")).unwrap();
        let recall = root.join("recall.dat");
        write_uniform(&recall, 2, 0xA2);
        let write = root.join("write.dat");
        write_uniform(&write, 1, 0x11);
        let dir = root.join("mod");
        fs::write(
            dir.join("intervals.json"),
            r#"[[{"start":0,"end":1},"Data"]]"#,
        )
        .unwrap();
        let opened = open_module(open_request(&dir, &recall, &write))
            .unwrap_or_else(|error| panic!("open: {error:#}"));
        let text = fs::read_to_string(dir.join("intervals.json")).unwrap();
        assert!(text.contains("\"Data\""), "{text}");
        assert!(!text.contains("Entropy"), "{text}");
        assert!(!opened.module.get_intervals(ChunkType::Data).is_empty());
        assert!(opened.module.get_intervals(ChunkType::Entropy).is_empty());
    }

    #[test]
    fn an_existing_uninitialized_intervals_file_becomes_entropy() {
        // Breaks if a file that is only Uninitialized is left in place.
        // That span is what a fresh open would reconcile, and the bench
        // replaces it so recall reads the entropy file.
        let tmp = TempDirBuilder::new().prefix("sm-intervals-uninit-").build();
        let root = tmp.path().join("loadtest");
        fs::create_dir_all(root.join("mod")).unwrap();
        let recall = root.join("recall.dat");
        write_uniform(&recall, 2, 0xA2);
        let write = root.join("write.dat");
        write_uniform(&write, 1, 0x11);
        let dir = root.join("mod");
        fs::write(
            dir.join("intervals.json"),
            r#"[[{"start":0,"end":1},"Uninitialized"]]"#,
        )
        .unwrap();
        let opened = open_module(open_request(&dir, &recall, &write))
            .unwrap_or_else(|error| panic!("open: {error:#}"));
        let text = fs::read_to_string(dir.join("intervals.json")).unwrap();
        assert!(text.contains("\"Entropy\""), "{text}");
        assert!(!text.contains("Uninitialized"), "{text}");
        assert_eq!(mining_mark(&opened.module, 0), 0xA2);
        assert!(
            opened
                .module
                .get_intervals(ChunkType::Uninitialized)
                .is_empty()
        );
    }
}
