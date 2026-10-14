//! Packed-chunk checksums stored in the offset index row.
//!
//! The checksum is CRC32C of the packed bytes just written to `chunks.dat`.
//! It is committed in the same index batch as the presence row, after
//! `chunks.dat` is `fdatasync`'d and before the interval label flips. A
//! missing checksum is unknown until `crc_complete` is set. After that flag
//! is set, a missing checksum is a mismatch.
//!
//! MDBX does not store the checksum and does not check one.

use std::{
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use opentelemetry::{KeyValue, global, metrics::Counter};

use super::ChunkType;

#[cfg(test)]
use std::cell::Cell;

#[derive(Clone, Copy)]
pub(super) enum CrcVerdict {
    Match,
    Unknown,
    Mismatch,
    Skip,
}

pub(super) fn checksum(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

/// `stored` is the checksum from the offset row. `None` means the row has no
/// checksum, or the row is absent.
pub(super) fn classify(stored: Option<u32>, bytes: &[u8], complete: bool) -> CrcVerdict {
    match stored {
        None if complete => CrcVerdict::Mismatch,
        None => CrcVerdict::Unknown,
        Some(stored) if stored == checksum(bytes) => CrcVerdict::Match,
        Some(_) => CrcVerdict::Mismatch,
    }
}

fn chunk_type_label(kind: ChunkType) -> &'static str {
    match kind {
        ChunkType::Data => "data",
        ChunkType::Entropy => "entropy",
        ChunkType::Uninitialized => "uninitialized",
        ChunkType::Interrupted => "interrupted",
    }
}

static UNKNOWN: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.chunk_crc_unknown_total")
        .with_description(
            "Settled chunks served without a stored checksum. Missing stays unknown until crc_complete is set.",
        )
        .build()
});

static MISMATCH: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.chunk_crc_mismatch_total")
        .with_description("Settled chunks whose packed bytes do not match the stored checksum")
        .build()
});

#[cfg(test)]
thread_local! {
    static UNKNOWN_LOCAL: Cell<u64> = const { Cell::new(0) };
    static MISMATCH_LOCAL: Cell<u64> = const { Cell::new(0) };
}

pub(super) fn note_unknown(caller: &'static str) {
    UNKNOWN.add(1, &[KeyValue::new("caller", caller)]);
    #[cfg(test)]
    UNKNOWN_LOCAL.with(|cell| cell.set(cell.get().saturating_add(1)));
}

pub(super) fn note_mismatch(caller: &'static str, kind: ChunkType) {
    MISMATCH.add(
        1,
        &[
            KeyValue::new("caller", caller),
            KeyValue::new("chunk_type", chunk_type_label(kind)),
        ],
    );
    #[cfg(test)]
    MISMATCH_LOCAL.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
pub(super) fn test_unknown_count() -> u64 {
    UNKNOWN_LOCAL.with(Cell::get)
}

#[cfg(test)]
pub(super) fn test_mismatch_count() -> u64 {
    MISMATCH_LOCAL.with(Cell::get)
}

static WARN_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
/// `WARN_NEVER` until the first warning. Starting at 0 would rate-limit the
/// first mismatch away: the origin is set by that same first call, so its
/// elapsed time is about 0 ms.
const WARN_NEVER: u64 = u64::MAX;
static WARN_LAST_MS: AtomicU64 = AtomicU64::new(WARN_NEVER);

/// At most one mismatch warning per second; the first one always logs.
fn mismatch_warning_due(now_ms: u64, last_ms: u64) -> bool {
    last_ms == WARN_NEVER || now_ms.saturating_sub(last_ms) >= 1_000
}

pub(super) fn warn_mismatch(module_id: usize, offset: u32) {
    let now_ms = u64::try_from(WARN_ORIGIN.elapsed().as_millis()).unwrap_or(u64::MAX);
    let prev = WARN_LAST_MS.load(Ordering::Relaxed);
    if !mismatch_warning_due(now_ms, prev) {
        return;
    }
    if WARN_LAST_MS
        .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    tracing::warn!(module_id, offset, "chunk crc mismatch");
}

#[cfg(test)]
mod tests {

    #[test]
    fn first_mismatch_warning_is_not_rate_limited() {
        use super::{WARN_NEVER, mismatch_warning_due};
        // The origin starts on the first call, so its elapsed time is ~0 ms.
        assert!(mismatch_warning_due(0, WARN_NEVER));
        assert!(!mismatch_warning_due(999, 0));
        assert!(mismatch_warning_due(1_000, 0));
        assert!(!mismatch_warning_due(1_500, 1_000));
    }
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::os::unix::fs::FileExt as _;

    use super::super::{ChunkType, StorageModule, StorageModuleInfo, SyncFailurePoint};
    use super::{checksum, classify, test_mismatch_count, test_unknown_count};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, DatabaseConfig, DbSyncMode, NodeConfig, PartitionChunkOffset,
        StorageSyncConfig, SubmoduleIndexEngine, partition_chunk_offset_ii,
    };
    use nodit::interval::ii;

    const CHUNK: usize = 32;
    const N: u64 = 8;
    const CRC_FILE: &str = "chunks.crc";

    fn index_database(engine: SubmoduleIndexEngine) -> DatabaseConfig {
        DatabaseConfig {
            sync_mode: DbSyncMode::UtterlyNoSync,
            cache_sync_mode: DbSyncMode::UtterlyNoSync,
            geometry_max_size: Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE),
            submodule_index: engine,
            ..DatabaseConfig::default()
        }
    }

    fn presence_info() -> StorageModuleInfo {
        StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
        }
    }

    fn open_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
        storage: StorageSyncConfig,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        irys_types::Config,
        StorageModule,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: CHUNK as u64,
                num_chunks_in_partition: N,
                ..ConsensusConfig::testing()
            }),
            storage,
            base_directory: tmp.path().to_path_buf(),
            database: index_database(engine),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(&presence_info(), &config)?;
        Ok((tmp, config, storage))
    }

    fn fast_storage() -> StorageSyncConfig {
        StorageSyncConfig {
            num_writes_before_sync: 1,
            intervals_persist_secs: 0,
            min_durable_interval_ms: 0,
            entropy_read_grace_ms: 0,
            ..StorageSyncConfig::default()
        }
    }

    fn sub_path(
        tmp: &irys_testing_utils::utils::tempfile::TempDir,
        name: &str,
    ) -> std::path::PathBuf {
        tmp.path().join("sub").join(name)
    }

    fn flip_dat(path: &std::path::Path, at: u64) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact_at(&mut byte, at).unwrap();
        byte[0] ^= 0xff;
        file.write_all_at(&byte, at).unwrap();
    }

    fn write_synced(module: &StorageModule, offset: u32, byte: u8, kind: ChunkType) {
        let bytes = vec![byte; CHUNK];
        assert!(module.write_chunk(PartitionChunkOffset::from(offset), bytes, kind));
        module.sync_pending_chunks().unwrap();
    }

    #[test]
    fn missing_checksum_is_unknown_until_complete_then_a_mismatch() {
        let bytes = [1_u8, 2, 3, 4];
        assert!(matches!(
            classify(None, &bytes, false),
            super::CrcVerdict::Unknown
        ));
        assert!(matches!(
            classify(None, &bytes, true),
            super::CrcVerdict::Mismatch
        ));
        let stored = checksum(&bytes);
        assert!(matches!(
            classify(Some(stored), &bytes, false),
            super::CrcVerdict::Match
        ));
        assert!(matches!(
            classify(Some(stored ^ 1), &bytes, true),
            super::CrcVerdict::Mismatch
        ));
        // A stored zero is a real checksum, not the unknown state.
        assert!(matches!(
            classify(Some(0), &bytes, false),
            super::CrcVerdict::Mismatch
        ));
    }

    #[test]
    fn rocks_data_and_entropy_rows_round_trip() -> eyre::Result<()> {
        let (tmp, _config, module) = open_module(
            "crc_round_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        let data = vec![0x11_u8; CHUNK];
        let entropy = vec![0x22_u8; CHUNK];
        assert!(module.write_chunk(PartitionChunkOffset::from(0), data.clone(), ChunkType::Data));
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            entropy.clone(),
            ChunkType::Entropy
        ));
        module.sync_pending_chunks()?;
        let data_row = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("data checksum row");
        let entropy_row = module
            .offset_meta_for_test(PartitionChunkOffset::from(1))?
            .expect("entropy checksum row");
        assert_eq!(data_row.crc, Some(checksum(&data)));
        assert!(data_row.proof.is_none(), "write_chunk stores no proof");
        assert_eq!(entropy_row.crc, Some(checksum(&entropy)));
        assert!(entropy_row.proof.is_none());
        let loaded = module.read_chunks(partition_chunk_offset_ii!(0, 1))?;
        assert_eq!(loaded.get(&PartitionChunkOffset::from(0)).unwrap().0, data);
        assert_eq!(
            loaded.get(&PartitionChunkOffset::from(1)).unwrap().0,
            entropy
        );
        assert!(!module.chunk_crc_complete_for_test());
        assert!(
            !sub_path(&tmp, CRC_FILE).exists(),
            "open does not create chunks.crc"
        );
        Ok(())
    }

    #[test]
    fn rocks_data_over_entropy_replaces_the_checksum() -> eyre::Result<()> {
        let (_tmp, _config, module) = open_module(
            "crc_replace_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        write_synced(&module, 0, 0x22, ChunkType::Entropy);
        let first = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("entropy row")
            .crc;
        write_synced(&module, 0, 0x33, ChunkType::Data);
        let second = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("data row");
        assert_ne!(first, second.crc);
        assert_eq!(second.crc, Some(checksum(&[0x33_u8; CHUNK])));
        assert!(second.proof.is_none());
        Ok(())
    }

    #[test]
    fn rocks_flipped_chunk_and_bad_row_are_omitted_and_queued() -> eyre::Result<()> {
        let (tmp, _config, module) = open_module(
            "crc_flip_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        write_synced(&module, 0, 0x31, ChunkType::Entropy);
        write_synced(&module, 1, 0x32, ChunkType::Entropy);
        flip_dat(&sub_path(&tmp, "chunks.dat"), 0);
        let before = test_mismatch_count();
        let recalled = module.read_recall_chunks(partition_chunk_offset_ii!(0, 1))?;
        assert!(!recalled.chunks.contains_key(&PartitionChunkOffset::from(0)));
        assert!(recalled.chunks.contains_key(&PartitionChunkOffset::from(1)));
        assert!(recalled.rows.contains_key(&PartitionChunkOffset::from(0)));
        assert!(test_mismatch_count() > before);
        let repairs = module.take_crc_repair_ranges();
        assert_eq!(repairs, vec![partition_chunk_offset_ii!(0, 0)]);
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );

        write_synced(&module, 0, 0x41, ChunkType::Data);
        module.set_offset_crc_for_test(PartitionChunkOffset::from(0), Some(0x1111_1111))?;
        let before = test_mismatch_count();
        let served = module.read_chunks(partition_chunk_offset_ii!(0, 1))?;
        assert!(!served.contains_key(&PartitionChunkOffset::from(0)));
        assert!(served.contains_key(&PartitionChunkOffset::from(1)));
        assert!(test_mismatch_count() > before);

        write_synced(&module, 2, 0x52, ChunkType::Data);
        module.set_offset_crc_for_test(PartitionChunkOffset::from(2), Some(0x2222_2222))?;
        let err = module
            .read_chunk_internal(PartitionChunkOffset::from(2))
            .unwrap_err();
        assert!(err.to_string().contains("chunk crc mismatch"), "{err}");
        assert!(!module.take_crc_repair_ranges().is_empty());
        Ok(())
    }

    #[test]
    fn rocks_missing_crc_is_unknown_until_complete_then_a_mismatch() -> eyre::Result<()> {
        let (_tmp, _config, module) = open_module(
            "crc_unknown_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        write_synced(&module, 0, 0x61, ChunkType::Data);
        assert!(!module.chunk_crc_complete_for_test());
        // Clearing the checksum on a checksum-only row deletes the row.
        module.set_offset_crc_for_test(PartitionChunkOffset::from(0), None)?;
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_none()
        );
        let before_unknown = test_unknown_count();
        let before_mismatch = test_mismatch_count();
        let served = module.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert!(served.contains_key(&PartitionChunkOffset::from(0)));
        assert!(test_unknown_count() > before_unknown);
        assert_eq!(test_mismatch_count(), before_mismatch);

        for offset in 0..N as u32 {
            write_synced(&module, offset, 0x70, ChunkType::Entropy);
        }
        assert!(module.chunk_crc_complete_for_test());
        module.set_offset_crc_for_test(PartitionChunkOffset::from(3), None)?;
        let before = test_mismatch_count();
        let served = module.read_chunks(partition_chunk_offset_ii!(3, 3))?;
        assert!(served.is_empty());
        assert!(test_mismatch_count() > before);
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(3)),
            Some(ChunkType::Uninitialized)
        );
        Ok(())
    }

    #[test]
    fn rocks_crash_before_fsync_reloads_uninitialized_without_a_false_mismatch() -> eyre::Result<()>
    {
        let (tmp, config, module) = open_module(
            "crc_crash_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        let bytes = vec![0x81_u8; CHUNK];
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            bytes.clone(),
            ChunkType::Entropy
        ));
        module.abandon_on_drop_for_test();
        module.fail_next_sync_at(SyncFailurePoint::BeforeDataFsync);
        let err = module.sync_pending_chunks().unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        drop(module);

        let reopened = StorageModule::new(&presence_info(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        assert!(
            reopened
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_none()
        );
        let before = test_mismatch_count();
        let served = reopened.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert!(served.is_empty());
        assert_eq!(test_mismatch_count(), before);
        assert!(reopened.write_chunk(
            PartitionChunkOffset::from(0),
            bytes.clone(),
            ChunkType::Entropy
        ));
        reopened.sync_pending_chunks()?;
        let served = reopened.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert_eq!(served.get(&PartitionChunkOffset::from(0)).unwrap().0, bytes);
        let row = reopened
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("repack stores a checksum");
        assert_eq!(row.crc, Some(checksum(&bytes)));
        assert!(!sub_path(&tmp, CRC_FILE).exists());
        Ok(())
    }

    #[test]
    fn rocks_reset_clears_complete_and_a_full_repack_sets_it() -> eyre::Result<()> {
        let (_tmp, _config, module) = open_module(
            "crc_reset_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        for offset in 0..N as u32 {
            write_synced(&module, offset, 0x91, ChunkType::Entropy);
        }
        assert!(module.chunk_crc_complete_for_test());
        module.reset()?;
        assert!(!module.chunk_crc_complete_for_test());
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_none()
        );
        for offset in 0..N as u32 {
            write_synced(&module, offset, 0x92, ChunkType::Entropy);
        }
        assert!(module.chunk_crc_complete_for_test());
        Ok(())
    }

    #[test]
    fn rocks_written_ahead_has_no_row_until_the_durable_point() -> eyre::Result<()> {
        let storage = StorageSyncConfig {
            min_durable_interval_ms: 60_000,
            ..fast_storage()
        };
        let (_tmp, _config, module) =
            open_module("crc_ahead_rocks", SubmoduleIndexEngine::Rocksdb, storage)?;
        write_synced(&module, 0, 0xc1, ChunkType::Entropy);
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0xc2_u8; CHUNK],
            ChunkType::Data
        ));
        module.sync_pending_chunks()?;
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(1))?
                .is_none(),
            "a deferred point does not commit the checksum"
        );
        let before = test_mismatch_count();
        let served = module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
        assert_eq!(
            served.get(&PartitionChunkOffset::from(1)).unwrap().0,
            vec![0xc2_u8; CHUNK]
        );
        assert_eq!(test_mismatch_count(), before);
        module.force_sync_pending_chunks()?;
        let row = module
            .offset_meta_for_test(PartitionChunkOffset::from(1))?
            .expect("durable point stores the checksum");
        assert_eq!(row.crc, Some(checksum(&[0xc2_u8; CHUNK])));
        module.set_offset_crc_for_test(PartitionChunkOffset::from(1), Some(0x3333_3333))?;
        let served = module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
        assert!(served.is_empty());
        assert!(test_mismatch_count() > before);
        Ok(())
    }

    #[test]
    fn rocks_reconcile_keeps_entropy_checksums_and_drops_uninitialized() -> eyre::Result<()> {
        let (tmp, config, module) = open_module(
            "crc_reconcile_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        write_synced(&module, 0, 0xd1, ChunkType::Entropy);
        write_synced(&module, 1, 0xd2, ChunkType::Data);
        let entropy_crc = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("entropy row")
            .crc;
        drop(module);

        let kept = StorageModule::new(&presence_info(), &config)?;
        assert_eq!(
            kept.offset_meta_for_test(PartitionChunkOffset::from(0))?
                .and_then(|row| row.crc),
            entropy_crc
        );
        assert!(
            kept.offset_meta_for_test(PartitionChunkOffset::from(1))?
                .expect("data span keeps the row")
                .proof
                .is_none()
        );
        drop(kept);

        let path = sub_path(&tmp, "intervals.json");
        let mut intervals = super::super::StorageIntervals::new();
        intervals
            .insert_merge_touching_if_values_equal(
                partition_chunk_offset_ii!(0, 7),
                ChunkType::Uninitialized,
            )
            .expect("uninitialized span");
        let mut file = super::super::get_atomic_file(&path)?;
        file.write_all(serde_json::to_string(&intervals)?.as_bytes())?;
        file.commit()?;
        let dropped = StorageModule::new(&presence_info(), &config)?;
        assert!(
            dropped
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_none()
        );
        assert!(
            dropped
                .offset_meta_for_test(PartitionChunkOffset::from(1))?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn rocks_checksum_only_row_is_not_a_data_chunk() -> eyre::Result<()> {
        let (tmp, config, module) = open_module(
            "crc_presence_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        let before_uncovered =
            super::super::super::index_read_metrics::uncovered_data_path_for_test();
        write_synced(&module, 2, 0xe4, ChunkType::Entropy);
        assert_eq!(
            super::super::super::index_read_metrics::uncovered_data_path_for_test(),
            before_uncovered,
            "a checksum-only row is not an uncovered proof"
        );
        let gaps_before = module.missing_path_hash_ranges(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(N as u32),
        )?;
        assert!(
            !gaps_before.is_empty(),
            "entropy does not cover a path-hash hole"
        );
        let paths = module
            .read_tx_data_paths(PartitionChunkOffset::from(2), PartitionChunkOffset::from(2))?;
        assert!(
            paths
                .get(&PartitionChunkOffset::from(2))
                .is_none_or(|(_, proof)| proof.is_none()),
            "an entropy offset has no proof: {paths:?}"
        );
        let served = module
            .generate_full_chunks(PartitionChunkOffset::from(2), PartitionChunkOffset::from(2))?;
        assert!(
            served.is_empty(),
            "serving an entropy offset returns no chunk"
        );
        drop(module);

        // A leftover sidecar is removed on open. The checksum row stays.
        std::fs::write(sub_path(&tmp, CRC_FILE), b"stale")?;
        let reopened = StorageModule::new(&presence_info(), &config)?;
        assert!(!sub_path(&tmp, CRC_FILE).exists());
        assert!(
            reopened
                .offset_meta_for_test(PartitionChunkOffset::from(2))?
                .and_then(|row| row.crc)
                .is_some()
        );
        let gaps_after = reopened.missing_path_hash_ranges(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(N as u32),
        )?;
        assert_eq!(gaps_before, gaps_after);
        Ok(())
    }

    #[test]
    fn mdbx_does_not_store_or_check_a_checksum() -> eyre::Result<()> {
        let (tmp, config, module) =
            open_module("crc_mdbx", SubmoduleIndexEngine::Mdbx, fast_storage())?;
        let bytes = vec![0xb2_u8; CHUNK];
        assert!(module.write_chunk(PartitionChunkOffset::from(1), bytes, ChunkType::Entropy));
        module.sync_pending_chunks()?;
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(1))?
                .is_none()
        );
        assert!(!module.chunk_crc_complete_for_test());
        assert!(!sub_path(&tmp, CRC_FILE).exists());
        flip_dat(&sub_path(&tmp, "chunks.dat"), CHUNK as u64);
        let before = test_mismatch_count();
        let served = module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
        assert!(served.contains_key(&PartitionChunkOffset::from(1)));
        assert_eq!(test_mismatch_count(), before);
        drop(module);
        std::fs::write(sub_path(&tmp, CRC_FILE), b"stale")?;
        let _reopened = StorageModule::new(&presence_info(), &config)?;
        assert!(!sub_path(&tmp, CRC_FILE).exists());
        Ok(())
    }

    /// Bytes read before a presence commit must not be judged against the
    /// checksum written by that commit. The read pauses after the pread.
    /// The durable point then commits. The stale bytes are a suspect only.
    #[test]
    fn rocks_bytes_from_before_presence_commit_are_not_a_mismatch() -> eyre::Result<()> {
        let (_tmp, _config, module) = open_module(
            "crc_stale_bytes",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        write_synced(&module, 0, 0x11, ChunkType::Entropy);
        let entropy_crc = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("entropy row")
            .crc;
        module.pause_next_crc_check();

        let (served, mismatches) = std::thread::scope(|scope| {
            struct Release<'a>(&'a StorageModule);
            impl Drop for Release<'_> {
                fn drop(&mut self) {
                    self.0.release_crc_check_pause();
                }
            }
            let _release = Release(&module);
            let reader = scope.spawn(|| {
                let before = test_mismatch_count();
                let served = module.read_chunks(partition_chunk_offset_ii!(0, 0));
                (served, test_mismatch_count().saturating_sub(before))
            });
            assert!(
                module.wait_for_crc_check_pause(std::time::Duration::from_secs(5)),
                "checksum check did not pause after the pread"
            );
            let data = vec![0x22_u8; CHUNK];
            assert!(module.write_chunk(
                PartitionChunkOffset::from(0),
                data.clone(),
                ChunkType::Data
            ));
            module.sync_pending_chunks().expect("presence commit");
            let row = module
                .offset_meta_for_test(PartitionChunkOffset::from(0))
                .expect("row read")
                .expect("data row");
            assert_eq!(row.crc, Some(checksum(&data)));
            assert_ne!(row.crc, entropy_crc);
            module.release_crc_check_pause();
            reader.join().expect("reader thread")
        });

        let served = served?;
        assert_eq!(mismatches, 0, "a stale pread is not a checksum mismatch");
        assert!(module.take_crc_repair_ranges().is_empty());
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Data)
        );
        assert_eq!(
            served
                .get(&PartitionChunkOffset::from(0))
                .map(|(bytes, _)| bytes.clone()),
            Some(vec![0x22_u8; CHUNK])
        );
        Ok(())
    }

    /// A stored data proof whose packed bytes were flipped is omitted and repaired.
    /// RocksDB Publish module with one durable data chunk at offset 0.
    fn durable_data_chunk_module(
        prefix: &str,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        use irys_types::ledger_chunk_offset_ii;
        use irys_types::partition::PartitionAssignment;
        use irys_types::{DataLedger, DataTransactionLedger, LedgerChunkOffset, LedgerChunkRange};

        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: CHUNK as u64,
                num_chunks_in_partition: N,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: fast_storage(),
            base_directory: tmp.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(0),
                    miner_address: config.node_config.miner_address(),
                    partition_hash: irys_types::H256::repeat_byte(0x11),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
            },
            &config,
        )?;
        let mut entropy = Vec::with_capacity(CHUNK);
        irys_packing::capacity_single::compute_entropy_chunk(
            config.node_config.miner_address(),
            0,
            module.partition_hash().expect("assignment").0,
            config.consensus.entropy_packing_iterations,
            CHUNK,
            &mut entropy,
            config.consensus.chain_id,
        );
        assert!(module.write_chunk(PartitionChunkOffset::from(0), entropy, ChunkType::Entropy));
        module.sync_pending_chunks()?;

        let signer = irys_types::irys::IrysSigner::random_signer(&config.consensus);
        let data = vec![0x5A_u8; CHUNK];
        let tx =
            signer.sign_transaction(signer.create_transaction(data, irys_types::H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        module.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        module.write_data_chunk(&unpacked[0])?;
        module.sync_pending_chunks()?;
        Ok((tmp, module))
    }

    #[test]
    fn rocks_pending_entropy_is_not_served_with_the_stored_data_proof() -> eyre::Result<()> {
        let (_tmp, module) = durable_data_chunk_module("crc_serve_pending")?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0xEE_u8; CHUNK],
            ChunkType::Entropy,
        ));
        let served = module
            .generate_full_chunks(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?;
        assert!(served.is_empty(), "unsettled bytes were served: {served:?}");
        module.sync_pending_chunks()?;
        let served = module
            .generate_full_chunks(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?;
        assert!(served.is_empty(), "entropy has no data proof: {served:?}");
        Ok(())
    }

    #[test]
    fn rocks_flipped_data_proof_is_omitted_from_generate_full_chunks() -> eyre::Result<()> {
        let (tmp, module) = durable_data_chunk_module("crc_serve_flip")?;
        let before_flip = module
            .generate_full_chunks(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?;
        assert_eq!(before_flip.len(), 1, "the stored proof is served");

        flip_dat(&sub_path(&tmp, "chunks.dat"), 0);
        let before = test_mismatch_count();
        let served = module
            .generate_full_chunks(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?;
        assert!(
            served.is_empty(),
            "a flipped packed chunk is not served: {served:?}"
        );
        assert!(test_mismatch_count() > before);
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        assert_eq!(
            module.take_crc_repair_ranges(),
            vec![partition_chunk_offset_ii!(0, 0)]
        );
        Ok(())
    }

    /// Entropy packed for hash A must not land after the module is hash B or unassigned.
    #[test]
    fn entropy_packed_for_another_partition_is_refused() -> eyre::Result<()> {
        use irys_types::H256;
        use irys_types::partition::PartitionAssignment;

        let tmp = TempDirBuilder::new().prefix("entropy_assignment").build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: CHUNK as u64,
                num_chunks_in_partition: N,
                ..ConsensusConfig::testing()
            }),
            storage: fast_storage(),
            base_directory: tmp.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, 7), tmp.path().join("sub"))],
            },
            &config,
        )?;
        let hash_a = H256::repeat_byte(0xA1);
        let hash_b = H256::repeat_byte(0xB2);
        let assign = |hash| PartitionAssignment {
            partition_hash: hash,
            miner_address: config.node_config.miner_address(),
            ledger_id: None,
            slot_index: None,
        };
        module.assign_partition(assign(hash_a), 1);
        let first = vec![0x11_u8; CHUNK];
        assert!(module.write_entropy_chunk(PartitionChunkOffset::from(0), first.clone(), hash_a));
        module.sync_pending_chunks()?;
        let served = module.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert_eq!(
            served
                .get(&PartitionChunkOffset::from(0))
                .map(|(bytes, _)| bytes.clone()),
            Some(first)
        );

        module.note_crc_mismatch(PartitionChunkOffset::from(0), ChunkType::Entropy, "test");
        assert!(!module.take_crc_repair_ranges().is_empty());
        module.note_crc_mismatch(PartitionChunkOffset::from(0), ChunkType::Entropy, "test");
        module.reset_with_assignment(Some(assign(hash_b)), 2)?;
        assert!(
            module.take_crc_repair_ranges().is_empty(),
            "reset drops repairs for the old bytes"
        );

        assert!(!module.write_entropy_chunk(
            PartitionChunkOffset::from(0),
            vec![0x33_u8; CHUNK],
            hash_a
        ));
        assert!(!module.has_pending_writes());
        module.sync_pending_chunks()?;
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_none()
        );
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );

        let fresh = vec![0x44_u8; CHUNK];
        assert!(module.write_entropy_chunk(PartitionChunkOffset::from(0), fresh.clone(), hash_b));
        module.sync_pending_chunks()?;
        let served = module.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert_eq!(
            served
                .get(&PartitionChunkOffset::from(0))
                .map(|(bytes, _)| bytes.clone()),
            Some(fresh)
        );

        module.reset_with_assignment(None, 3)?;
        assert!(!module.write_entropy_chunk(
            PartitionChunkOffset::from(1),
            vec![0x55_u8; CHUNK],
            hash_b
        ));
        assert!(!module.has_pending_writes());
        Ok(())
    }

    fn open_assigned(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        irys_types::Config,
        StorageModule,
    )> {
        use irys_types::partition::PartitionAssignment;

        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: CHUNK as u64,
                num_chunks_in_partition: N,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: fast_storage(),
            base_directory: tmp.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(irys_types::DataLedger::Publish.into()),
                    slot_index: Some(0),
                    miner_address: config.node_config.miner_address(),
                    partition_hash: irys_types::H256::repeat_byte(0x11),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
            },
            &config,
        )?;
        Ok((tmp, config, module))
    }

    fn index_one_chunk(
        module: &StorageModule,
        config: &irys_types::Config,
        bytes: Vec<u8>,
    ) -> eyre::Result<irys_types::UnpackedChunk> {
        use irys_types::ledger_chunk_offset_ii;
        use irys_types::{DataTransactionLedger, LedgerChunkOffset, LedgerChunkRange};

        let signer = irys_types::irys::IrysSigner::random_signer(&config.consensus);
        let tx =
            signer.sign_transaction(signer.create_transaction(bytes, irys_types::H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        module.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        Ok(unpacked[0].clone())
    }

    #[test]
    fn rocks_offset_crc_missing_reports_a_gap_and_a_present_span() -> eyre::Result<()> {
        let (_tmp, _config, module) = open_module(
            "crc_missing_rocks",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        assert!(
            !module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(1),
                PartitionChunkOffset::from(0),
            )?,
            "an empty span is not a missing checksum"
        );
        assert!(
            module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(7),
            )?,
            "a span with no rows is a gap"
        );
        for offset in 0..N as u32 {
            write_synced(&module, offset, 0x41, ChunkType::Entropy);
        }
        assert!(
            !module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(7),
            )?,
            "every settled offset has a checksum"
        );
        module.set_offset_crc_for_test(PartitionChunkOffset::from(3), None)?;
        assert!(
            module
                .offset_meta_for_test(PartitionChunkOffset::from(3))?
                .is_none(),
            "a checksum-only row with no checksum is deleted"
        );
        assert!(
            module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(7),
            )?,
            "a deleted row is a gap"
        );
        assert!(
            !module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(2),
            )?,
            "a span whose rows all have a checksum is complete"
        );
        Ok(())
    }

    #[test]
    fn rocks_proof_only_row_keeps_crc_complete_false() -> eyre::Result<()> {
        let (_tmp, config, module) = open_assigned("crc_proof_only")?;
        write_synced(&module, 0, 0x21, ChunkType::Entropy);
        let chunk = index_one_chunk(&module, &config, vec![0x5A_u8; CHUNK])?;
        module
            .write_data_chunk(&chunk)
            .expect("pack the data chunk");
        module.sync_pending_chunks()?;
        for offset in 1..N as u32 {
            write_synced(&module, offset, 0x22, ChunkType::Entropy);
        }
        assert!(module.chunk_crc_complete_for_test());
        module.set_offset_crc_for_test(PartitionChunkOffset::from(0), None)?;
        let row = module
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("the proof stays");
        assert!(row.crc.is_none());
        assert!(row.proof.is_some());
        assert!(
            module.offset_crc_missing_for_test(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(7),
            )?,
            "a proof with no checksum is missing"
        );
        module.clear_chunk_crc_complete()?;
        assert!(
            !module
                .crc_scan_done
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        write_synced(&module, 1, 0x23, ChunkType::Entropy);
        assert!(
            module
                .crc_scan_done
                .load(std::sync::atomic::Ordering::SeqCst),
            "the scan ran"
        );
        assert!(
            !module.chunk_crc_complete_for_test(),
            "one settled offset with no checksum keeps crc_complete false"
        );
        Ok(())
    }

    #[test]
    fn rocks_flipped_entropy_is_not_packed_by_the_sweep() -> eyre::Result<()> {
        let (tmp, config, module) = open_assigned("crc_sweep_flip")?;
        write_synced(&module, 0, 0x31, ChunkType::Entropy);
        let chunk = index_one_chunk(&module, &config, vec![0x5A_u8; CHUNK])?;
        flip_dat(&sub_path(&tmp, "chunks.dat"), 0);
        let err = module
            .write_data_chunk(&chunk)
            .expect_err("a flipped entropy chunk is not packed");
        assert!(err.to_string().contains("chunk crc mismatch"), "{err}");
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        assert!(!module.has_pending_writes());
        assert_eq!(
            module.take_crc_repair_ranges(),
            vec![partition_chunk_offset_ii!(0, 0)]
        );
        Ok(())
    }

    #[test]
    fn reset_during_interval_snapshot_keeps_the_file_uninitialized() -> eyre::Result<()> {
        let (tmp, _config, module) = open_module(
            "interval_epoch",
            SubmoduleIndexEngine::Rocksdb,
            fast_storage(),
        )?;
        for offset in 0..N as u32 {
            write_synced(&module, offset, 0x44, ChunkType::Data);
        }
        let path = sub_path(&tmp, "intervals.json");
        let before = super::super::read_intervals_file(&path)?;
        assert!(
            before.iter().any(|(_, kind)| *kind == ChunkType::Data),
            "the writer copies a Data image"
        );
        module.pause_next_interval_image();
        std::thread::scope(|scope| -> eyre::Result<()> {
            struct Release<'a>(&'a StorageModule);
            impl Drop for Release<'_> {
                fn drop(&mut self) {
                    self.0.release_interval_image_pause();
                }
            }
            let _release = Release(&module);
            let writer = scope.spawn(|| module.write_intervals_to_submodules());
            assert!(
                module.wait_for_interval_image_pause(std::time::Duration::from_secs(5)),
                "interval rewrite did not pause after the map copy"
            );
            module.reset()?;
            let mid = super::super::read_intervals_file(&path)?;
            assert!(
                mid.iter()
                    .all(|(_, kind)| *kind == ChunkType::Uninitialized),
                "reset publishes Uninitialized"
            );
            module.release_interval_image_pause();
            writer.join().expect("interval writer")?;
            let after = super::super::read_intervals_file(&path)?;
            assert!(
                after
                    .iter()
                    .all(|(_, kind)| *kind == ChunkType::Uninitialized),
                "the old snapshot must not replace the reset image"
            );
            Ok(())
        })?;
        Ok(())
    }
}
