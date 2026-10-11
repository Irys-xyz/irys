//! Sidecar `chunks.crc` beside `chunks.dat`.
//!
//! The file has no state of its own. It follows the `chunks.dat` interval
//! machine: entries are pwritten from the same buffers, and both files are
//! `fdatasync`'d before a completion label or a presence row. A check runs
//! only when the loaded label is `Data` or `Entropy` and the offset is not
//! in `pending.chunks` or `written_ahead`.
//!
//! # Layout (little-endian)
//!
//! | Offset | Bytes | Field |
//! | --- | --- | --- |
//! | `0x00` | 8 | magic `IRYSCRCC` |
//! | `0x08` | 4 | format version (`1`) |
//! | `0x0C` | 4 | chunk size |
//! | `0x10` | 8 | chunk count |
//! | `0x18` | 4 | flags, bit 0 = `crc_complete` |
//! | `0x1C` | 4 | CRC32C of bytes `[0x00, 0x1C)` |
//! | `0x20 + i*4` | 4 | entry for submodule index `i` |
//!
//! An entry stores `crc32c(chunk) ^ 0xC32C_C32C`. `fallocate` zeros the file,
//! so a stored `0` means the chunk has no checksum yet. A real CRC equal to
//! the constant also stores as `0` (one value in `2^32`). While `crc_complete`
//! is unset that chunk is not checked. Once the flag is set, a stored `0` is
//! a mismatch: the chunk is not served. The other encoding (map `0` to a
//! reserved value) would treat a real CRC of that value as valid and could
//! serve corrupt bytes.
//!
//! A header that fails the magic, version, size, count, or checksum check is
//! rewritten with `crc_complete` unset. Entry bytes are left in place.

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::FileExt as _,
    path::{Path, PathBuf},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use eyre::{Context as _, eyre};
use opentelemetry::{KeyValue, global, metrics::Counter};

use super::ChunkType;

#[cfg(test)]
use std::cell::Cell;

pub(super) const FILE_NAME: &str = "chunks.crc";
pub(super) const HEADER_LEN: u64 = 32;
const MAGIC: [u8; 8] = *b"IRYSCRCC";
const VERSION: u32 = 1;
const FLAG_COMPLETE: u32 = 1;
/// Nonzero mask. `stored = crc32c(bytes) ^ CRC_XOR`. Stored `0` is unknown.
const CRC_XOR: u32 = 0xC32C_C32C;
const WARN_INTERVAL_MS: u64 = 1_000;

pub(super) enum CrcVerdict {
    Match,
    Unknown,
    Mismatch,
    Skip,
}

pub(super) struct ChunkCrc {
    file: Mutex<File>,
    path: PathBuf,
    chunk_size: u32,
    chunk_count: u64,
    complete: AtomicBool,
}

pub(super) fn file_len(chunk_count: u64) -> eyre::Result<u64> {
    chunk_count
        .checked_mul(4)
        .and_then(|entries| entries.checked_add(HEADER_LEN))
        .ok_or_else(|| eyre!("chunks.crc length overflows for {chunk_count} chunks"))
}

pub(super) fn encode_crc(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes) ^ CRC_XOR
}

pub(super) fn classify(stored: u32, bytes: &[u8], complete: bool) -> CrcVerdict {
    if stored == 0 {
        if complete {
            CrcVerdict::Mismatch
        } else {
            CrcVerdict::Unknown
        }
    } else if stored == encode_crc(bytes) {
        CrcVerdict::Match
    } else {
        CrcVerdict::Mismatch
    }
}

impl ChunkCrc {
    pub(super) fn open(dir: &Path, chunk_size: u64, chunk_count: u64) -> eyre::Result<Self> {
        let chunk_size = u32::try_from(chunk_size)
            .wrap_err_with(|| format!("chunk size {chunk_size} does not fit in the crc header"))?;
        let full_len = file_len(chunk_count)?;
        let path = dir.join(FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .wrap_err_with(|| format!("Failed to create or open {}", path.display()))?;
        super::allocate_chunks_file(&file, &path, full_len)
            .wrap_err_with(|| format!("Failed to allocate {}", path.display()))?;
        super::advise_chunks_random(&file, &path)?;
        let complete = load_or_rebuild_header(&file, &path, chunk_size, chunk_count)?;
        tracing::info!(
            path = %path.display(),
            bytes = full_len,
            complete,
            "opened chunks.crc"
        );
        Ok(Self {
            file: Mutex::new(file),
            path,
            chunk_size,
            chunk_count,
            complete: AtomicBool::new(complete),
        })
    }

    pub(super) fn is_complete(&self) -> bool {
        self.complete.load(Ordering::SeqCst)
    }

    /// One `pwrite` of the encoded entries for a contiguous run.
    pub(super) fn write_entries(&self, start_index: u64, chunk_bytes: &[u8]) -> eyre::Result<()> {
        let chunk_len = self.chunk_size as usize;
        if chunk_len == 0 || !chunk_bytes.len().is_multiple_of(chunk_len) {
            return Err(eyre!(
                "crc write of {} bytes is not a multiple of chunk size {}",
                chunk_bytes.len(),
                self.chunk_size
            ));
        }
        let count = (chunk_bytes.len() / chunk_len) as u64;
        let end = start_index
            .checked_add(count)
            .ok_or_else(|| eyre!("crc entry index overflow"))?;
        if end > self.chunk_count {
            return Err(eyre!(
                "crc entries {start_index}..{end} exceed {} chunks",
                self.chunk_count
            ));
        }
        let mut encoded = Vec::with_capacity(count as usize * 4);
        for chunk in chunk_bytes.chunks(chunk_len) {
            encoded.extend_from_slice(&encode_crc(chunk).to_le_bytes());
        }
        let at = HEADER_LEN + start_index * 4;
        let file = lock_file(&self.file);
        file.write_all_at(&encoded, at).wrap_err_with(|| {
            format!(
                "crc pwrite at entry {start_index} on {}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    pub(super) fn read_entries(&self, start_index: u64, count: u64) -> eyre::Result<Vec<u32>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let end = start_index
            .checked_add(count)
            .ok_or_else(|| eyre!("crc entry index overflow"))?;
        if end > self.chunk_count {
            return Err(eyre!(
                "crc read {start_index}..{end} exceeds {} chunks",
                self.chunk_count
            ));
        }
        let bytes = usize::try_from(count * 4).wrap_err("crc read is larger than memory")?;
        let mut buf = vec![0_u8; bytes];
        let at = HEADER_LEN + start_index * 4;
        let file = lock_file(&self.file);
        file.read_exact_at(&mut buf, at).wrap_err_with(|| {
            format!("crc read at entry {start_index} on {}", self.path.display())
        })?;
        drop(file);
        let mut entries = Vec::with_capacity(count as usize);
        for word in buf.chunks_exact(4) {
            entries.push(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
        }
        Ok(entries)
    }

    pub(super) fn sync_data(&self) -> eyre::Result<()> {
        let file = lock_file(&self.file);
        file.sync_data()
            .wrap_err_with(|| format!("fdatasync {}", self.path.display()))
    }

    pub(super) fn set_complete(&self) -> eyre::Result<()> {
        self.write_header(FLAG_COMPLETE)?;
        self.complete.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub(super) fn clear_complete(&self) -> eyre::Result<()> {
        self.write_header(0)?;
        self.complete.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// `true` when any stored entry is zero. One pass, after the module is settled.
    pub(super) fn any_zero_entry(&self) -> eyre::Result<bool> {
        let file = {
            let guard = lock_file(&self.file);
            guard
                .try_clone()
                .wrap_err_with(|| format!("dup {} for the crc scan", self.path.display()))?
        };
        let end = file_len(self.chunk_count)?;
        let mut buf = vec![0_u8; 1024 * 1024];
        let mut at = HEADER_LEN;
        while at < end {
            let want = usize::try_from(end - at).unwrap_or(buf.len());
            let mut n = want.min(buf.len());
            n -= n % 4;
            if n == 0 {
                break;
            }
            file.read_exact_at(&mut buf[..n], at)
                .wrap_err_with(|| format!("crc scan of {}", self.path.display()))?;
            if buf[..n].chunks_exact(4).any(|word| word == [0, 0, 0, 0]) {
                return Ok(true);
            }
            at += n as u64;
        }
        Ok(false)
    }

    fn write_header(&self, flags: u32) -> eyre::Result<()> {
        let bytes = header_bytes(self.chunk_size, self.chunk_count, flags);
        let file = lock_file(&self.file);
        file.write_all_at(&bytes, 0)
            .wrap_err_with(|| format!("crc header write on {}", self.path.display()))?;
        file.sync_data()
            .wrap_err_with(|| format!("fdatasync crc header {}", self.path.display()))?;
        Ok(())
    }
}

impl std::fmt::Debug for ChunkCrc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChunkCrc")
            .field("path", &self.path)
            .field("chunk_size", &self.chunk_size)
            .field("chunk_count", &self.chunk_count)
            .field("complete", &self.is_complete())
            .finish_non_exhaustive()
    }
}

fn lock_file(file: &Mutex<File>) -> std::sync::MutexGuard<'_, File> {
    file.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn header_bytes(chunk_size: u32, chunk_count: u64, flags: u32) -> [u8; 32] {
    let mut buf = [0_u8; 32];
    buf[..8].copy_from_slice(&MAGIC);
    buf[8..12].copy_from_slice(&VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&chunk_size.to_le_bytes());
    buf[16..24].copy_from_slice(&chunk_count.to_le_bytes());
    buf[24..28].copy_from_slice(&flags.to_le_bytes());
    let sum = crc32c::crc32c(&buf[..28]);
    buf[28..32].copy_from_slice(&sum.to_le_bytes());
    buf
}

fn load_or_rebuild_header(
    file: &File,
    path: &Path,
    chunk_size: u32,
    chunk_count: u64,
) -> eyre::Result<bool> {
    let mut buf = [0_u8; 32];
    file.read_exact_at(&mut buf, 0)
        .wrap_err_with(|| format!("read crc header {}", path.display()))?;
    if let Some(complete) = parse_header(&buf, chunk_size, chunk_count) {
        return Ok(complete);
    }
    tracing::warn!(
        path = %path.display(),
        "chunks.crc header is invalid; rewriting it with crc_complete unset and keeping entries"
    );
    let bytes = header_bytes(chunk_size, chunk_count, 0);
    file.write_all_at(&bytes, 0)
        .wrap_err_with(|| format!("rewrite crc header {}", path.display()))?;
    file.sync_data()
        .wrap_err_with(|| format!("fdatasync rebuilt crc header {}", path.display()))?;
    Ok(false)
}

fn parse_header(buf: &[u8; 32], chunk_size: u32, chunk_count: u64) -> Option<bool> {
    if buf[..8] != MAGIC {
        return None;
    }
    let version = read_u32(buf, 8);
    let size = read_u32(buf, 12);
    let count = u64::from_le_bytes([
        buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
    ]);
    let flags = read_u32(buf, 24);
    let sum = read_u32(buf, 28);
    if version != VERSION || size != chunk_size || count != chunk_count {
        return None;
    }
    if crc32c::crc32c(&buf[..28]) != sum {
        return None;
    }
    Some(flags & FLAG_COMPLETE != 0)
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
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
            "Settled chunks served without a crc entry. A stored zero is unknown until crc_complete is set.",
        )
        .build()
});

static MISMATCH: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.chunk_crc_mismatch_total")
        .with_description("Settled chunks whose bytes do not match the crc entry")
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
static WARN_LAST_MS: AtomicU64 = AtomicU64::new(0);

pub(super) fn warn_mismatch(module_id: usize, offset: u32) {
    let now_ms = u64::try_from(WARN_ORIGIN.elapsed().as_millis()).unwrap_or(u64::MAX);
    let prev = WARN_LAST_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) < WARN_INTERVAL_MS {
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
    use super::*;

    #[test]
    fn encode_does_not_use_zero_for_an_empty_buffer() {
        assert_ne!(encode_crc(&[]), 0);
        assert_eq!(CRC_XOR ^ CRC_XOR, 0);
    }

    #[test]
    fn stored_zero_is_unknown_until_complete_then_a_mismatch() {
        let bytes = [1_u8, 2, 3, 4];
        assert!(matches!(classify(0, &bytes, false), CrcVerdict::Unknown));
        assert!(matches!(classify(0, &bytes, true), CrcVerdict::Mismatch));
        let stored = encode_crc(&bytes);
        assert_ne!(stored, 0);
        assert!(matches!(classify(stored, &bytes, false), CrcVerdict::Match));
        assert!(matches!(
            classify(stored ^ 1, &bytes, true),
            CrcVerdict::Mismatch
        ));
    }

    #[test]
    fn header_round_trip_and_rejected_checksum() {
        let bytes = header_bytes(32, 8, FLAG_COMPLETE);
        assert_eq!(parse_header(&bytes, 32, 8), Some(true));
        assert_eq!(parse_header(&bytes, 32, 9), None);
        let mut broken = bytes;
        broken[31] ^= 0xff;
        assert_eq!(parse_header(&broken, 32, 8), None);
        let mut flags = bytes;
        flags[24] = 0;
        // The checksum covers the flags, so a hand edit is not a valid header.
        assert_eq!(parse_header(&flags, 32, 8), None);
        let cleared = header_bytes(32, 8, 0);
        assert_eq!(parse_header(&cleared, 32, 8), Some(false));
    }

    use std::fs::{File, OpenOptions};

    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, DatabaseConfig, DbSyncMode, NodeConfig, PartitionChunkOffset,
        StorageSyncConfig, SubmoduleIndexEngine, partition_chunk_offset_ii,
    };
    use nodit::interval::ii;

    use super::super::{ChunkType, StorageModule, StorageModuleInfo, SyncFailurePoint};
    use super::{FILE_NAME, HEADER_LEN, encode_crc, test_mismatch_count, test_unknown_count};

    const CHUNK: usize = 32;
    const N: u64 = 8;

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
            ..StorageSyncConfig::default()
        }
    }

    fn engines() -> [SubmoduleIndexEngine; 2] {
        [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb]
    }

    fn sub_path(
        tmp: &irys_testing_utils::utils::tempfile::TempDir,
        name: &str,
    ) -> std::path::PathBuf {
        tmp.path().join("sub").join(name)
    }

    fn poke_entry(path: &std::path::Path, index: u64, value: u32) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.write_all_at(&value.to_le_bytes(), HEADER_LEN + index * 4)
            .unwrap();
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
    fn data_and_entropy_writes_store_entries_and_reads_match() -> eyre::Result<()> {
        for engine in engines() {
            let (tmp, _config, module) =
                open_module(&format!("crc_round_{engine:?}"), engine, fast_storage())?;
            let data = vec![0x11_u8; CHUNK];
            let entropy = vec![0x22_u8; CHUNK];
            assert!(module.write_chunk(
                PartitionChunkOffset::from(0),
                data.clone(),
                ChunkType::Data
            ));
            assert!(module.write_chunk(
                PartitionChunkOffset::from(1),
                entropy.clone(),
                ChunkType::Entropy
            ));
            module.sync_pending_chunks()?;
            let crc_path = sub_path(&tmp, FILE_NAME);
            let file = File::open(&crc_path)?;
            let mut word = [0_u8; 4];
            file.read_exact_at(&mut word, HEADER_LEN)?;
            assert_eq!(u32::from_le_bytes(word), encode_crc(&data));
            file.read_exact_at(&mut word, HEADER_LEN + 4)?;
            assert_eq!(u32::from_le_bytes(word), encode_crc(&entropy));
            let loaded = module.read_chunks(partition_chunk_offset_ii!(0, 1))?;
            assert_eq!(loaded.get(&PartitionChunkOffset::from(0)).unwrap().0, data);
            assert_eq!(
                loaded.get(&PartitionChunkOffset::from(1)).unwrap().0,
                entropy
            );
            assert!(!module.chunk_crc_complete_for_test());
        }
        Ok(())
    }

    #[test]
    fn flipped_chunk_and_bad_entry_are_omitted_and_queued() -> eyre::Result<()> {
        for engine in engines() {
            let (tmp, _config, module) =
                open_module(&format!("crc_flip_{engine:?}"), engine, fast_storage())?;
            write_synced(&module, 0, 0x31, ChunkType::Entropy);
            write_synced(&module, 1, 0x32, ChunkType::Entropy);
            flip_dat(&sub_path(&tmp, "chunks.dat"), 0);
            let before = test_mismatch_count();
            let recalled = module.read_recall_chunks(partition_chunk_offset_ii!(0, 1))?;
            assert!(!recalled.contains_key(&PartitionChunkOffset::from(0)));
            assert!(recalled.contains_key(&PartitionChunkOffset::from(1)));
            assert!(test_mismatch_count() > before);
            let repairs = module.take_crc_repair_ranges();
            assert_eq!(repairs, vec![partition_chunk_offset_ii!(0, 0)]);
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset::from(0)),
                Some(ChunkType::Uninitialized)
            );

            write_synced(&module, 0, 0x41, ChunkType::Data);
            poke_entry(&sub_path(&tmp, FILE_NAME), 0, 0x1111_1111);
            let before = test_mismatch_count();
            let served = module.read_chunks(partition_chunk_offset_ii!(0, 1))?;
            assert!(!served.contains_key(&PartitionChunkOffset::from(0)));
            assert!(served.contains_key(&PartitionChunkOffset::from(1)));
            assert!(test_mismatch_count() > before);
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset::from(0)),
                Some(ChunkType::Uninitialized)
            );
            write_synced(&module, 2, 0x52, ChunkType::Data);
            poke_entry(&sub_path(&tmp, FILE_NAME), 2, 0x2222_2222);
            let err = module
                .read_chunk_internal(PartitionChunkOffset::from(2))
                .unwrap_err();
            assert!(err.to_string().contains("chunk crc mismatch"), "{err}");
            assert!(!module.take_crc_repair_ranges().is_empty());
        }
        Ok(())
    }

    #[test]
    fn zero_entry_is_unknown_until_complete_then_a_mismatch() -> eyre::Result<()> {
        for engine in engines() {
            let (tmp, _config, module) =
                open_module(&format!("crc_zero_{engine:?}"), engine, fast_storage())?;
            write_synced(&module, 0, 0x61, ChunkType::Data);
            assert!(!module.chunk_crc_complete_for_test());
            poke_entry(&sub_path(&tmp, FILE_NAME), 0, 0);
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
            poke_entry(&sub_path(&tmp, FILE_NAME), 3, 0);
            let before = test_mismatch_count();
            let served = module.read_chunks(partition_chunk_offset_ii!(3, 3))?;
            assert!(served.is_empty());
            assert!(test_mismatch_count() > before);
            assert_eq!(
                module.get_chunk_type(&PartitionChunkOffset::from(3)),
                Some(ChunkType::Uninitialized)
            );
        }
        Ok(())
    }

    #[test]
    fn crash_before_fsync_reloads_uninitialized_and_repack_rewrites_the_entry() -> eyre::Result<()>
    {
        for engine in engines() {
            let (tmp, config, module) =
                open_module(&format!("crc_crash_{engine:?}"), engine, fast_storage())?;
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
            let file = File::open(sub_path(&tmp, FILE_NAME))?;
            let mut word = [0_u8; 4];
            file.read_exact_at(&mut word, HEADER_LEN)?;
            assert_eq!(u32::from_le_bytes(word), encode_crc(&bytes));
            drop(reopened);
        }
        Ok(())
    }

    #[test]
    fn reset_clears_complete_and_a_full_repack_sets_it() -> eyre::Result<()> {
        for engine in engines() {
            let (_tmp, _config, module) =
                open_module(&format!("crc_reset_{engine:?}"), engine, fast_storage())?;
            for offset in 0..N as u32 {
                write_synced(&module, offset, 0x91, ChunkType::Entropy);
            }
            assert!(module.chunk_crc_complete_for_test());
            module.reset()?;
            assert!(!module.chunk_crc_complete_for_test());
            for offset in 0..N as u32 {
                write_synced(&module, offset, 0x92, ChunkType::Entropy);
            }
            assert!(module.chunk_crc_complete_for_test());
        }
        Ok(())
    }

    #[test]
    fn corrupt_header_is_rebuilt_and_entries_still_match() -> eyre::Result<()> {
        for engine in engines() {
            let (tmp, config, module) =
                open_module(&format!("crc_header_{engine:?}"), engine, fast_storage())?;
            let bytes = vec![0xa1_u8; CHUNK];
            write_synced(&module, 4, 0xa1, ChunkType::Data);
            drop(module);
            let path = sub_path(&tmp, FILE_NAME);
            {
                let file = OpenOptions::new().write(true).open(&path)?;
                file.write_all_at(&[0x00], 0)?;
            }
            let reopened = StorageModule::new(&presence_info(), &config)?;
            assert!(!reopened.chunk_crc_complete_for_test());
            let mut magic = [0_u8; 8];
            File::open(&path)?.read_exact_at(&mut magic, 0)?;
            assert_eq!(&magic, b"IRYSCRCC");
            let before = test_unknown_count();
            let served = reopened.read_chunks(partition_chunk_offset_ii!(4, 4))?;
            assert_eq!(served.get(&PartitionChunkOffset::from(4)).unwrap().0, bytes);
            assert_eq!(test_unknown_count(), before);
        }
        Ok(())
    }

    #[test]
    fn missing_crc_file_is_created_and_chunks_stay_unknown() -> eyre::Result<()> {
        for engine in engines() {
            let (tmp, config, module) =
                open_module(&format!("crc_missing_{engine:?}"), engine, fast_storage())?;
            let bytes = vec![0xb2_u8; CHUNK];
            write_synced(&module, 1, 0xb2, ChunkType::Entropy);
            drop(module);
            let path = sub_path(&tmp, FILE_NAME);
            std::fs::remove_file(&path)?;
            let reopened = StorageModule::new(&presence_info(), &config)?;
            assert!(path.is_file());
            assert_eq!(std::fs::metadata(&path)?.len(), HEADER_LEN + 4 * N);
            assert!(!reopened.chunk_crc_complete_for_test());
            let before = test_unknown_count();
            let served = reopened.read_chunks(partition_chunk_offset_ii!(1, 1))?;
            assert_eq!(served.get(&PartitionChunkOffset::from(1)).unwrap().0, bytes);
            assert!(test_unknown_count() > before);
        }
        Ok(())
    }

    #[test]
    fn written_ahead_bytes_are_not_checked_until_the_durable_point() -> eyre::Result<()> {
        for engine in engines() {
            let storage = StorageSyncConfig {
                min_durable_interval_ms: 60_000,
                ..fast_storage()
            };
            let (tmp, _config, module) =
                open_module(&format!("crc_ahead_{engine:?}"), engine, storage)?;
            write_synced(&module, 0, 0xc1, ChunkType::Entropy);
            assert!(module.write_chunk(
                PartitionChunkOffset::from(1),
                vec![0xc2_u8; CHUNK],
                ChunkType::Data
            ));
            module.sync_pending_chunks()?;
            poke_entry(&sub_path(&tmp, FILE_NAME), 1, 0x3333_3333);
            let before = test_mismatch_count();
            let served = module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
            assert_eq!(
                served.get(&PartitionChunkOffset::from(1)).unwrap().0,
                vec![0xc2_u8; CHUNK]
            );
            assert_eq!(test_mismatch_count(), before);
            module.force_sync_pending_chunks()?;
            let served = module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
            assert!(served.is_empty());
            assert!(test_mismatch_count() > before);
        }
        Ok(())
    }

    #[test]
    fn measure_crc32c_recall_and_durable_point() -> eyre::Result<()> {
        use std::time::Instant;

        const CHUNK_BYTES: usize = 256 * 1024;
        const RECALL: usize = 400;
        const ROUNDS: u32 = 20;

        let mut sample = vec![0x5a_u8; CHUNK_BYTES];
        sample[0] = 1;
        let started = Instant::now();
        let mut acc = 0_u32;
        for step in 0..64_u32 {
            sample[2] = step as u8;
            acc = acc.wrapping_add(std::hint::black_box(encode_crc(&sample)));
        }
        let crc_nanos = started.elapsed().as_nanos();
        assert_ne!(std::hint::black_box(acc), 0);
        let crc_bytes = 64_u128 * CHUNK_BYTES as u128;
        let crc_mib_s = crc_bytes.saturating_mul(1_000_000_000) / crc_nanos.max(1) / (1024 * 1024);

        let tmp = TempDirBuilder::new().prefix("crc_measure").build();
        let data_path = tmp.path().join("chunks.dat");
        let crc_path = tmp.path().join("chunks.crc");
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&data_path)?;
        let crc = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&crc_path)?;
        let chunk_count = RECALL as u64;
        data.set_len(chunk_count * CHUNK_BYTES as u64)?;
        let entries_len = HEADER_LEN + chunk_count * 4;
        crc.set_len(entries_len)?;
        let mut encoded = Vec::with_capacity(RECALL * 4);
        for index in 0..RECALL {
            sample[1] = index as u8;
            data.write_all_at(&sample, index as u64 * CHUNK_BYTES as u64)?;
            encoded.extend_from_slice(&encode_crc(&sample).to_le_bytes());
        }
        crc.write_all_at(&encoded, HEADER_LEN)?;
        data.sync_all()?;
        crc.sync_all()?;

        let time_recall = |data: &File, crc: &File| -> eyre::Result<(u128, Vec<u8>)> {
            let started = Instant::now();
            let mut words = vec![0_u8; RECALL * 4];
            crc.read_exact_at(&mut words, HEADER_LEN)?;
            // One pread, matching a recall range. Per-chunk preads are not
            // what `read_recall_run` issues.
            let mut buf = vec![0_u8; RECALL * CHUNK_BYTES];
            data.read_exact_at(&mut buf, 0)?;
            for index in 0..RECALL {
                let at = index * CHUNK_BYTES;
                let stored = u32::from_le_bytes([
                    words[index * 4],
                    words[index * 4 + 1],
                    words[index * 4 + 2],
                    words[index * 4 + 3],
                ]);
                assert_eq!(stored, encode_crc(&buf[at..at + CHUNK_BYTES]));
            }
            Ok((started.elapsed().as_nanos(), buf))
        };
        let time_crc_resident = |crc: &File, buf: &[u8]| -> eyre::Result<u128> {
            let started = Instant::now();
            let mut words = vec![0_u8; RECALL * 4];
            crc.read_exact_at(&mut words, HEADER_LEN)?;
            for index in 0..RECALL {
                let at = index * CHUNK_BYTES;
                let stored = u32::from_le_bytes([
                    words[index * 4],
                    words[index * 4 + 1],
                    words[index * 4 + 2],
                    words[index * 4 + 3],
                ]);
                assert_eq!(stored, encode_crc(&buf[at..at + CHUNK_BYTES]));
            }
            Ok(started.elapsed().as_nanos())
        };
        let _ = time_recall(&data, &crc)?;
        let (warm_nanos, warm_buf) = time_recall(&data, &crc)?;
        let crc_resident_nanos = time_crc_resident(&crc, &warm_buf)?;

        let dontneed = |file: &File, len: u64| {
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::io::AsRawFd as _;
                let rc = unsafe {
                    libc::posix_fadvise(
                        file.as_raw_fd(),
                        0,
                        libc::off_t::try_from(len).unwrap_or(libc::off_t::MAX),
                        libc::POSIX_FADV_DONTNEED,
                    )
                };
                assert_eq!(rc, 0, "posix_fadvise(DONTNEED)");
            }
            #[cfg(not(target_os = "linux"))]
            let _ = (file, len);
        };
        dontneed(&data, chunk_count * CHUNK_BYTES as u64);
        dontneed(&crc, entries_len);
        let (cold_nanos, _) = time_recall(&data, &crc)?;

        let sync_once = |extra_crc: bool| -> eyre::Result<u128> {
            let path = tmp.path().join(if extra_crc {
                "sync-crc.dat"
            } else {
                "sync.dat"
            });
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)?;
            let side = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(
                    tmp.path()
                        .join(if extra_crc { "sync.crc" } else { "sync.none" }),
                )?;
            let mut samples = Vec::with_capacity(ROUNDS as usize);
            for round in 0..ROUNDS {
                let started = Instant::now();
                file.write_all_at(&sample, u64::from(round) * CHUNK_BYTES as u64)?;
                file.sync_data()?;
                if extra_crc {
                    side.write_all_at(&encode_crc(&sample).to_le_bytes(), u64::from(round) * 4)?;
                    side.sync_data()?;
                }
                samples.push(started.elapsed().as_nanos());
            }
            samples.sort_unstable();
            Ok(samples[samples.len() / 2])
        };
        let data_only = sync_once(false)?;
        let with_crc = sync_once(true)?;

        let report = format!(
            "crc32c_256kib_mib_s={crc_mib_s} crc32c_iters=64 crc32c_nanos={crc_nanos}\n\
             recall_400_warm_nanos={warm_nanos}\n\
             recall_400_crc_resident_nanos={crc_resident_nanos}\n\
             recall_400_cold_nanos={cold_nanos}\n\
             durable_data_fdatasync_median_nanos={data_only}\n\
             durable_data_and_crc_fdatasync_median_nanos={with_crc}\n\
             path={}\n",
            tmp.path().display()
        );
        std::fs::write("/tmp/chunk_crc_measure.txt", &report)?;
        assert!(crc_mib_s > 0);
        assert!(warm_nanos > 0);
        assert!(cold_nanos > 0);
        Ok(())
    }
}
