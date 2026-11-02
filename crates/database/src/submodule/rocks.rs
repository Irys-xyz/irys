//! RocksDB submodule index.
//!
//! One database, one column family per table. `update` holds the write mutex
//! and commits a `WriteBatch` only when the closure returns `Ok`. Reads in
//! that closure see the overlay. `view` uses a snapshot and does not take
//! the mutex.
//!
//! Gap scans walk placement intervals. With an overlay, key counts are not
//! O(1), so a heal scan reads the intervals that touch the window.
//!
//! Path values stay in SST blocks so LZ4 can share neighbouring proofs.
//! The `blobs` preset extracts values at or above [`BLOB_MIN_BYTES`].
//! Compaction inlines an old blob file. A trivial move does not.
//! rust-rocksdb 0.24 cannot clear `allow_fallocate`. WAL recycle,
//! `wal_bytes_per_sync`, `use_fsync`, and `max_total_wal_size` are the
//! durability knobs this binding exposes. Every open keeps SST readers
//! (`max_open_files=-1`): a finite cap drops table readers and reloads the
//! filter. Production shares one block cache, one write-buffer manager, and
//! the process Env pools. There is no separate blob cache for new values.
//! A bench preset can keep a private cache.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eyre::WrapErr as _;
use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, DatabaseConfig, H256, PartitionChunkOffset, TxPath,
    TxPathHash,
};
use reth_db::table::{Decode, Encode, Table};
use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, DB, DBCompactionStyle, DBCompressionType, Direction,
    IteratorMode, Options, ReadOptions, WriteBatch, WriteOptions,
};

use super::group::{EnableStep, EngineLifetime, GroupCommit};
use super::interval::{IntervalRow, coalesce_inclusive_spans, combine_hashes, coverage_gaps};
use super::packing::{PackingRecord, packing_params_valid_for_module};
use super::records::{self, IndexLayout};
use super::store::PresenceReconcile;
use super::tables::{
    CHUNK_META_BY_OFFSET_CF, ChunkPathHashes, DataRootInfo, DataRootInfos, OffsetMeta,
    PackedOffsetRow, PendingBodyMigration, Placement, SUBMODULE_SCHEMA_ROCKS_V1, TxLeafBinding,
    unrecognised_rocks_layout,
};
use super::{
    SubmoduleRead, SubmoduleStore as _, SubmoduleWrite, tables::Metadata as MetadataTable,
};
use crate::metadata::MetadataKey;

/// User-data block size. Uncompressed. Reads of the offset index pull this much.
const BLOCK_BYTES: usize = 64 * 1024;
/// Private block cache for an open that opts out of [`RocksResources`].
/// Production baseline does not use this size. Inline path blocks use this
/// cache on that open. It is not a separate blob-cache budget.
pub const BLOCK_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Uncompressed size at or above which the `blobs` preset writes a path
/// value to a blob file. Baseline stores that value in the SST.
pub const BLOB_MIN_BYTES: u64 = 2048;
/// Blob GC cutoff when new blob files are off. 1.0 marks every existing
/// blob file old, so a rewrite inlines it. A trivial move does not.
const LEGACY_BLOB_GC_AGE_CUTOFF: f64 = 1.0;
/// Per-database background-job cap. The process Env pools are separate.
/// The bench can select another preset.
const BACKGROUND_JOBS: i32 = 2;
/// Refill period for an optional flush and compaction rate limiter.
const RATE_LIMIT_REFILL_PERIOD_US: i64 = 100_000;
/// Fairness of that limiter. Matches the binding's example.
const RATE_LIMIT_FAIRNESS: i32 = 10;
/// Leveled SST target. Compaction output preallocation uses this as a cap.
const TARGET_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Live WAL cap for the rotate presets. Small enough that a bench step
/// switches logs and then runs on reused files. Each switch flushes every
/// family that still holds data in the old log.
const WAL_ROTATE_BYTES: u64 = 1024 * 1024;
/// Live SST files on one column family that start a whole-range compact
/// in `fdatasync-wal-rotate-compact`. Baseline keeps `0` (the hook off).
const SMALL_FILE_COMPACT_THRESHOLD: usize = 64;
/// How often the compact thread rechecks live SST counts.
const SMALL_FILE_COMPACT_POLL: Duration = Duration::from_millis(100);
/// Levels counted by `rocksdb.num-files-at-level<N>` (RocksDB default).
const SST_LEVELS: usize = 7;
const SCHEMA_FILE: &str = "SCHEMA";

/// Live files in one column family after a bench settle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnFileUsage {
    pub name: String,
    pub sst_files: u64,
    pub sst_bytes: u64,
    pub blob_files: u64,
    pub blob_bytes: u64,
}

/// One RocksDB open. Production uses [`RocksTuning::baseline`].
///
/// Each other preset changes one field, except `full-filters`,
/// `fdatasync-wal-rotate`, and `fdatasync-wal-rotate-compact`.
/// `share_resources` selects the process cache and is not one of those
/// fields. Options are fixed at open. An existing SST keeps its filter
/// until compaction rewrites it. A family with no filter policy skips a
/// stored filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RocksTuning {
    pub name: &'static str,
    pub block_bytes: usize,
    pub block_cache_bytes: usize,
    /// `2^bits` block-cache shards. `-1` caps at 64 shards. `0` is one shard.
    /// A filter block must fit in one shard or RocksDB inserts it and drops it.
    pub cache_shard_bits: i32,
    /// `true` is LZ4 on SST blocks and, when blob files are on, on blob files.
    /// `false` stores both uncompressed.
    pub compress: bool,
    /// Blob files for new path values at or above [`BLOB_MIN_BYTES`].
    /// `false` keeps them in the SST. An existing blob file is still read,
    /// and compaction inlines it.
    pub blobs: bool,
    /// Direct I/O for flush and compaction. Reads stay buffered.
    pub direct_io: bool,
    pub background_jobs: i32,
    pub target_file_bytes: u64,
    /// Universal compaction. `false` is leveled compaction.
    pub universal: bool,
    /// Partitioned filters and a two-level index. `false` is one full filter
    /// per SST, which does not fit in one shard of the 64 MiB cache.
    pub partition_filters: bool,
    /// Retired WAL files to overwrite. Baseline is 4. `0` allocates a new file.
    ///
    /// A `disableWAL` write is rejected while this count is non-zero.
    /// The converting open sets it to 0. `WAL_ttl_seconds` or
    /// `WAL_size_limit_MB` would also force it to 0. Recovery stays
    /// `kPointInTimeRecovery`, which keeps a non-zero count.
    pub recycle_log_file_num: usize,
    /// `true` uses `fsync` for the WAL, MANIFEST, SST, and blob files.
    /// `false` uses `fdatasync`. RocksDB treats them as equally durable.
    /// Directory sync uses `FsyncWithDirOptions` and ignores this flag.
    /// Baseline is `true`.
    pub use_fsync: bool,
    /// Live WAL bytes that force a memtable switch and a flush.
    /// `0` lets RocksDB pick about 4 GiB, so a bench step never rotates.
    /// RocksDB applies a non-zero cap only when there is more than one
    /// column family. The switch flushes every family that still references
    /// the oldest log.
    pub max_total_wal_size: u64,
    /// Live SST files that start one `compact_range` with bottommost force.
    /// `0` leaves the hook off. The call runs off the write lock, one at a time.
    pub small_file_compact_threshold: usize,
    /// `true`: 10-bit bloom on every family. `false`: only families whose
    /// point lookups can miss. `ChunkOffsetsByContentHash` always uses a
    /// 32-byte prefix bloom. A whole-key bloom cannot answer that seek.
    /// Does not change `partition_filters`.
    pub bloom_all_families: bool,
    /// Bloom on `ChunkMetaByOffset`. A proof write reads the stored row first,
    /// and a new offset misses. Off by default: a full-partition measurement
    /// did not change proof-write time. The `datapath-bloom` preset turns it on.
    pub data_path_bloom: bool,
    /// Process-wide cache, write buffer, and Env pools. `false` builds a
    /// private cache of `block_cache_bytes` and skips the write-buffer manager.
    /// Preset diffs do not count this flag.
    pub share_resources: bool,
}

impl RocksTuning {
    pub const fn baseline() -> Self {
        Self {
            name: "baseline",
            block_bytes: BLOCK_BYTES,
            block_cache_bytes: BLOCK_CACHE_BYTES,
            cache_shard_bits: -1,
            compress: true,
            blobs: false,
            direct_io: false,
            background_jobs: BACKGROUND_JOBS,
            target_file_bytes: TARGET_FILE_BYTES,
            universal: false,
            partition_filters: true,
            recycle_log_file_num: 4,
            use_fsync: true,
            max_total_wal_size: 0,
            small_file_compact_threshold: 0,
            bloom_all_families: false,
            data_path_bloom: false,
            share_resources: true,
        }
    }

    /// Named opens. `baseline` matches [`Self::baseline`].
    pub const fn presets() -> [Self; 16] {
        let base = Self::baseline();
        [
            base,
            Self {
                name: "block-16k",
                block_bytes: 16 * 1024,
                ..base
            },
            Self {
                name: "block-256k",
                block_bytes: 256 * 1024,
                ..base
            },
            Self {
                name: "cache-1g",
                block_cache_bytes: 1024 * 1024 * 1024,
                share_resources: false,
                ..base
            },
            Self {
                name: "blobs",
                blobs: true,
                ..base
            },
            Self {
                name: "no-compress",
                compress: false,
                ..base
            },
            Self {
                name: "direct-io",
                direct_io: true,
                ..base
            },
            Self {
                name: "universal",
                universal: true,
                ..base
            },
            // One full filter. One shard so the filter can use the whole
            // cache. 64 shards would turn 128 MiB into 2 MiB shards and
            // drop the block on insert.
            Self {
                name: "full-filters",
                partition_filters: false,
                block_cache_bytes: 128 * 1024 * 1024,
                cache_shard_bits: 0,
                share_resources: false,
                ..base
            },
            // RocksDB 10.4 does not scale this count with buffers or families.
            Self {
                name: "no-wal-recycle",
                recycle_log_file_num: 0,
                ..base
            },
            Self {
                name: "fdatasync-wal",
                use_fsync: false,
                ..base
            },
            // A full log switches, and the next log reuses that file.
            Self {
                name: "fdatasync-wal-rotate",
                use_fsync: false,
                max_total_wal_size: WAL_ROTATE_BYTES,
                ..base
            },
            // Same cap with baseline `fsync`, so a run can separate the two.
            Self {
                name: "fsync-wal-rotate",
                max_total_wal_size: WAL_ROTATE_BYTES,
                ..base
            },
            // Rotate plus a count-triggered CompactRange. WAL switches flush
            // tiny memtables. Offset keys do not overlap, so those files
            // trivial-move to L6 and stay there. Force-rewriting the bottom
            // level merges them once a family has more than 64 live SSTs.
            Self {
                name: "fdatasync-wal-rotate-compact",
                use_fsync: false,
                max_total_wal_size: WAL_ROTATE_BYTES,
                small_file_compact_threshold: SMALL_FILE_COMPACT_THRESHOLD,
                ..base
            },
            Self {
                name: "all-blooms",
                bloom_all_families: true,
                ..base
            },
            Self {
                name: "datapath-bloom",
                data_path_bloom: true,
                ..base
            },
        ]
    }

    pub fn preset(name: &str) -> eyre::Result<Self> {
        Self::presets()
            .into_iter()
            .find(|preset| preset.name == name)
            .ok_or_else(|| eyre::eyre!("unknown rocks preset {name}; use {}", Self::preset_names()))
    }

    pub fn preset_names() -> String {
        Self::presets()
            .iter()
            .map(|preset| preset.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Replace the cache after a preset is chosen. The name stays.
    ///
    /// The replacement is private. The process cache would ignore `bytes`.
    pub fn with_block_cache(mut self, bytes: usize) -> Self {
        self.block_cache_bytes = bytes;
        self.share_resources = false;
        self
    }

    /// Replace the small-file compact threshold after a preset is chosen.
    /// `0` leaves the hook off. The name stays.
    pub fn with_small_file_compact_threshold(mut self, threshold: usize) -> Self {
        self.small_file_compact_threshold = threshold;
        self
    }

    pub fn describe(self) -> String {
        let compression = if self.compress { "lz4" } else { "none" };
        let blob = if self.blobs {
            format!("on blob_min_bytes={BLOB_MIN_BYTES}")
        } else {
            "off".to_string()
        };
        let direct_io = if self.direct_io { "on" } else { "off" };
        let compaction = if self.universal { "universal" } else { "level" };
        let filters = if self.partition_filters {
            "partitioned"
        } else {
            "full"
        };
        let blooms = if self.bloom_all_families {
            "all"
        } else if self.data_path_bloom {
            "miss+datapath"
        } else {
            "miss"
        };
        let shared = if self.share_resources { "on" } else { "off" };
        format!(
            "rocks={} recycle_log_file_num={} use_fsync={} max_total_wal_size={} small_file_compact_threshold={} compression={compression} block_bytes={} blob={blob} direct_io={direct_io} compaction={compaction} filters={filters} blooms={blooms} jobs={} target_file_bytes={} block_cache_bytes={} cache_shard_bits={} shared_resources={shared}",
            self.name,
            self.recycle_log_file_num,
            self.use_fsync,
            self.max_total_wal_size,
            self.small_file_compact_threshold,
            self.block_bytes,
            self.background_jobs,
            self.target_file_bytes,
            self.block_cache_bytes,
            self.cache_shard_bits,
        )
    }

    /// Table knobs only. The name and `share_resources` are not knobs.
    /// `share_resources` selects the process cache. It does not change an SST.
    #[cfg(test)]
    fn knob_diffs(self, other: Self) -> usize {
        usize::from(self.block_bytes != other.block_bytes)
            + usize::from(self.block_cache_bytes != other.block_cache_bytes)
            + usize::from(self.cache_shard_bits != other.cache_shard_bits)
            + usize::from(self.compress != other.compress)
            + usize::from(self.blobs != other.blobs)
            + usize::from(self.direct_io != other.direct_io)
            + usize::from(self.background_jobs != other.background_jobs)
            + usize::from(self.target_file_bytes != other.target_file_bytes)
            + usize::from(self.universal != other.universal)
            + usize::from(self.partition_filters != other.partition_filters)
            + usize::from(self.recycle_log_file_num != other.recycle_log_file_num)
            + usize::from(self.use_fsync != other.use_fsync)
            + usize::from(self.max_total_wal_size != other.max_total_wal_size)
            + usize::from(self.small_file_compact_threshold != other.small_file_compact_threshold)
            + usize::from(self.bloom_all_families != other.bloom_all_families)
            + usize::from(self.data_path_bloom != other.data_path_bloom)
    }
}

/// Sizes for the process-wide RocksDB cache, write buffer, and Env pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RocksResourceLimits {
    pub block_cache_bytes: u64,
    pub write_buffer_bytes: u64,
    pub flush_threads: u32,
    pub compaction_threads: u32,
    /// `0` does not install a rate limiter.
    pub rate_limit_bytes_per_sec: u64,
}

impl RocksResourceLimits {
    pub const fn production() -> Self {
        Self {
            block_cache_bytes: irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES,
            write_buffer_bytes: irys_types::DEFAULT_ROCKSDB_WRITE_BUFFER_BYTES,
            flush_threads: irys_types::DEFAULT_ROCKSDB_FLUSH_THREADS,
            compaction_threads: irys_types::DEFAULT_ROCKSDB_COMPACTION_THREADS,
            rate_limit_bytes_per_sec: irys_types::DEFAULT_ROCKSDB_RATE_LIMIT_BYTES_PER_SEC,
        }
    }

    pub fn from_database(config: &DatabaseConfig) -> eyre::Result<Self> {
        let limits = Self {
            block_cache_bytes: config.rocksdb_block_cache_bytes,
            write_buffer_bytes: config.rocksdb_write_buffer_bytes,
            flush_threads: config.rocksdb_flush_threads,
            compaction_threads: config.rocksdb_compaction_threads,
            rate_limit_bytes_per_sec: config.rocksdb_rate_limit_bytes_per_sec,
        };
        limits.check()?;
        Ok(limits)
    }

    fn check(self) -> eyre::Result<()> {
        eyre::ensure!(
            self.block_cache_bytes > 0,
            "rocksdb_block_cache_bytes must be greater than 0"
        );
        eyre::ensure!(
            self.write_buffer_bytes > 0,
            "rocksdb_write_buffer_bytes must be greater than 0"
        );
        eyre::ensure!(
            self.write_buffer_bytes <= self.block_cache_bytes,
            "rocksdb_write_buffer_bytes ({}) exceeds rocksdb_block_cache_bytes ({})",
            self.write_buffer_bytes,
            self.block_cache_bytes
        );
        eyre::ensure!(
            self.flush_threads >= 1,
            "rocksdb_flush_threads must be greater than 0"
        );
        eyre::ensure!(
            self.compaction_threads >= 1,
            "rocksdb_compaction_threads must be greater than 0"
        );
        usize::try_from(self.block_cache_bytes)
            .wrap_err("rocksdb_block_cache_bytes does not fit in usize")?;
        usize::try_from(self.write_buffer_bytes)
            .wrap_err("rocksdb_write_buffer_bytes does not fit in usize")?;
        i32::try_from(self.flush_threads).wrap_err("rocksdb_flush_threads does not fit in i32")?;
        i32::try_from(self.compaction_threads)
            .wrap_err("rocksdb_compaction_threads does not fit in i32")?;
        if self.rate_limit_bytes_per_sec > 0 {
            i64::try_from(self.rate_limit_bytes_per_sec)
                .wrap_err("rocksdb_rate_limit_bytes_per_sec does not fit in i64")?;
        }
        Ok(())
    }

    fn flush_pool(self) -> i32 {
        i32::try_from(self.flush_threads).expect("flush threads checked")
    }

    fn compaction_pool(self) -> i32 {
        i32::try_from(self.compaction_threads).expect("compaction threads checked")
    }
}

/// Shared block cache, write-buffer manager, and process Env. Pools only grow.
///
/// The manager charges memtables to the cache and does not stall writers: a
/// stall waits for every writer that shares it. Each family keeps about 1 MiB
/// of arena that a flush replaces, so a stall cannot reclaim that floor.
/// Forty databases and seven families are about 280 MiB of the 512 MiB budget.
/// Path bytes sit in that arena until flush whether blob files are on.
pub struct RocksResources {
    limits: RocksResourceLimits,
    cache: rocksdb::Cache,
    write_buffer: rocksdb::WriteBufferManager,
    env: rocksdb::Env,
}

impl RocksResources {
    pub fn new(limits: RocksResourceLimits) -> eyre::Result<Arc<Self>> {
        limits.check()?;
        let cache_bytes = usize::try_from(limits.block_cache_bytes)
            .wrap_err("rocksdb_block_cache_bytes does not fit in usize")?;
        let mut cache_opts = rocksdb::LruCacheOptions::default();
        cache_opts.set_capacity(cache_bytes);
        // `-1` caps at 64 shards. 1 GiB is then 16 MiB per shard, which holds
        // a partitioned filter block. One shard would serialize every lookup.
        cache_opts.set_num_shard_bits(-1);
        let cache = rocksdb::Cache::new_lru_cache_opts(&cache_opts);
        let write_bytes = usize::try_from(limits.write_buffer_bytes)
            .wrap_err("rocksdb_write_buffer_bytes does not fit in usize")?;
        let write_buffer = rocksdb::WriteBufferManager::new_write_buffer_manager_with_cache(
            write_bytes,
            false,
            cache.clone(),
        );
        let env = rocksdb::Env::new().wrap_err("open the default rocksdb env")?;
        raise_default_env_threads(&env, limits.flush_pool(), limits.compaction_pool());
        Ok(Arc::new(Self {
            limits,
            cache,
            write_buffer,
            env,
        }))
    }

    pub fn limits(&self) -> RocksResourceLimits {
        self.limits
    }

    pub fn block_cache_usage(&self) -> u64 {
        u64::try_from(self.cache.get_usage()).unwrap_or(u64::MAX)
    }

    pub fn block_cache_pinned_usage(&self) -> u64 {
        u64::try_from(self.cache.get_pinned_usage()).unwrap_or(u64::MAX)
    }

    pub fn block_cache_capacity(&self) -> u64 {
        self.limits.block_cache_bytes
    }

    pub fn write_buffer_usage(&self) -> u64 {
        u64::try_from(self.write_buffer.get_usage()).unwrap_or(u64::MAX)
    }

    pub fn write_buffer_capacity(&self) -> u64 {
        u64::try_from(self.write_buffer.get_buffer_size()).unwrap_or(u64::MAX)
    }
}

/// First successful install wins. A later call with different sizes keeps
/// that set and logs the mismatch. Thread pools still only grow.
static PROCESS_ROCKS_RESOURCES: OnceLock<Arc<RocksResources>> = OnceLock::new();
/// Highest flush and compaction pool sizes this process has requested.
/// `SetBackgroundThreads` can shrink a pool. A later open must not do that.
static ENV_THREAD_POOLS: Mutex<(i32, i32)> = Mutex::new((0, 0));

/// Install the process set from node config. The first call creates it.
/// Later calls keep the first cache and write buffer.
pub fn install_process_rocks_resources(config: &DatabaseConfig) -> eyre::Result<()> {
    let limits = RocksResourceLimits::from_database(config)?;
    if let Some(existing) = PROCESS_ROCKS_RESOURCES.get() {
        note_existing_resources(existing, limits);
        return Ok(());
    }
    let created = RocksResources::new(limits)?;
    match PROCESS_ROCKS_RESOURCES.set(created) {
        Ok(()) => Ok(()),
        Err(loser) => {
            if let Some(existing) = PROCESS_ROCKS_RESOURCES.get() {
                note_existing_resources(existing, loser.limits);
            }
            Ok(())
        }
    }
}

fn note_existing_resources(existing: &RocksResources, requested: RocksResourceLimits) {
    if existing.limits != requested {
        tracing::warn!(
            installed_block_cache_bytes = existing.limits.block_cache_bytes,
            requested_block_cache_bytes = requested.block_cache_bytes,
            installed_write_buffer_bytes = existing.limits.write_buffer_bytes,
            requested_write_buffer_bytes = requested.write_buffer_bytes,
            "rocksdb process resources already installed; keeping the first set"
        );
    }
    raise_default_env_threads(
        &existing.env,
        requested.flush_pool(),
        requested.compaction_pool(),
    );
}

fn process_rocks_resources() -> eyre::Result<Arc<RocksResources>> {
    if let Some(existing) = PROCESS_ROCKS_RESOURCES.get() {
        return Ok(Arc::clone(existing));
    }
    let created = RocksResources::new(RocksResourceLimits::production())?;
    Ok(Arc::clone(PROCESS_ROCKS_RESOURCES.get_or_init(|| created)))
}

fn raise_default_env_threads(env: &rocksdb::Env, flush: i32, compaction: i32) {
    let mut guard = ENV_THREAD_POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Clone so the setters can take `&mut`. Both wrappers point at the
    // process default Env. The bottom pool stays at zero.
    let mut env = env.clone();
    if flush > guard.0 {
        env.set_high_priority_background_threads(flush);
        guard.0 = flush;
    }
    if compaction > guard.1 {
        env.set_background_threads(compaction);
        guard.1 = compaction;
    }
}

const CF_COUNT: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cf {
    /// Offset row in `ChunkMetaByOffset`. The value is the versioned proof
    /// and checksum.
    OffsetMeta,
    DataRoots,
    Pending,
    Interval,
    Metadata,
    /// content_hash (32 bytes) ‖ partition offset (u32 big-endian). Empty value.
    Content,
}

impl Cf {
    const ALL: [Self; CF_COUNT] = [
        Self::OffsetMeta,
        Self::DataRoots,
        Self::Pending,
        Self::Interval,
        Self::Metadata,
        Self::Content,
    ];

    fn index(self) -> usize {
        match self {
            Self::OffsetMeta => 0,
            Self::DataRoots => 1,
            Self::Pending => 2,
            Self::Interval => 3,
            Self::Metadata => 4,
            Self::Content => 5,
        }
    }

    /// On-disk family name. RocksDB v1 has one name per family.
    fn name(self) -> &'static str {
        match self {
            Self::Interval => "PlacementIntervals",
            Self::DataRoots => "DataRootPlacements",
            Self::Pending => "PendingBodyMigrations",
            Self::OffsetMeta => CHUNK_META_BY_OFFSET_CF,
            Self::Metadata => <MetadataTable as Table>::NAME,
            Self::Content => "ChunkOffsetsByContentHash",
        }
    }

    /// Offset rows may still live in a blob file from an earlier open.
    /// Baseline stores new values inline and garbage-collects that file.
    /// The `blobs` preset still writes one.
    fn uses_blobs(self) -> bool {
        matches!(self, Self::OffsetMeta)
    }

    /// A bloom can reject a miss. `bloom_all_families` forces one elsewhere.
    /// [`Self::Content`] always has a prefix bloom. Data-root reads are
    /// prefix seeks, and a whole-key bloom cannot answer them.
    fn uses_bloom(self, tuning: &RocksTuning) -> bool {
        matches!(self, Self::Content)
            || tuning.bloom_all_families
            || (tuning.data_path_bloom && matches!(self, Self::OffsetMeta))
    }
}

/// Count-triggered whole-range compact. One thread per database.
///
/// `compact_range_cf` runs on RocksDB's pool. This thread waits. The write
/// mutex stays free. `threshold == 0` skips the thread.
struct SmallFileCompactor {
    state: Arc<CompactState>,
    join: Mutex<Option<JoinHandle<()>>>,
}

struct CompactState {
    stop: AtomicBool,
    wake: Condvar,
    wait: Mutex<()>,
    compactions: AtomicU64,
    files_before: AtomicU64,
    files_after: AtomicU64,
}

impl CompactState {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            wake: Condvar::new(),
            wait: Mutex::new(()),
            compactions: AtomicU64::new(0),
            files_before: AtomicU64::new(0),
            files_after: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.compactions.load(Ordering::Relaxed),
            self.files_before.load(Ordering::Relaxed),
            self.files_after.load(Ordering::Relaxed),
        )
    }
}

impl SmallFileCompactor {
    fn start(db: Arc<DB>, threshold: usize, submodule: String) -> eyre::Result<Arc<Self>> {
        let state = Arc::new(CompactState::new());
        let this = Arc::new(Self {
            state: Arc::clone(&state),
            join: Mutex::new(None),
        });
        if threshold == 0 {
            return Ok(this);
        }
        let join = std::thread::Builder::new()
            .name("irys-idx-compact".into())
            .spawn(move || run_small_file_compact(&state, &db, threshold, &submodule))
            .wrap_err("spawn submodule index small-file compact thread")?;
        {
            let mut slot = this
                .join
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.replace(join);
        }
        Ok(this)
    }
}

impl Drop for SmallFileCompactor {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.state.wake.notify_all();
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(join) = join
            && join.join().is_err()
        {
            tracing::error!("submodule index small-file compact thread panicked");
        }
    }
}

fn run_small_file_compact(state: &CompactState, db: &DB, threshold: usize, submodule: &str) {
    loop {
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        compact_small_files(state, db, threshold, submodule);
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        let guard = state
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        let (_guard, _) = state
            .wake
            .wait_timeout(guard, SMALL_FILE_COMPACT_POLL)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn compact_small_files(state: &CompactState, db: &DB, threshold: usize, submodule: &str) {
    if threshold == 0 {
        return;
    }
    let limit = threshold as u64;
    for cf in families() {
        if state.stop.load(Ordering::Acquire) {
            return;
        }
        let Ok(handle) = cf_handle(db, cf) else {
            continue;
        };
        let before = live_sst_count(db, handle);
        if before <= limit {
            continue;
        }
        // Live SST bytes of this family before the rewrite. That is the
        // input the compact reads.
        let bytes = live_sst_bytes(db, handle);
        let started = Instant::now();
        let mut opts = rocksdb::CompactOptions::default();
        opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
        opts.set_exclusive_manual_compaction(false);
        db.compact_range_cf_opt(&handle, None::<&[u8]>, None::<&[u8]>, &opts);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let after = live_sst_count(db, handle);
        state.compactions.fetch_add(1, Ordering::Relaxed);
        state.files_before.fetch_add(before, Ordering::Relaxed);
        state.files_after.fetch_add(after, Ordering::Relaxed);
        tracing::info!(
            submodule,
            cf = cf.name(),
            files_before = before,
            files_after = after,
            bytes,
            elapsed_ms,
            "small-file compact_range"
        );
    }
}

fn live_sst_count(db: &DB, handle: &rocksdb::ColumnFamily) -> u64 {
    let mut total = 0_u64;
    for level in 0..SST_LEVELS {
        if let Ok(Some(count)) =
            db.property_int_value_cf(handle, rocksdb::properties::num_files_at_level(level))
        {
            total = total.saturating_add(count);
        }
    }
    total
}

fn live_sst_bytes(db: &DB, handle: &rocksdb::ColumnFamily) -> u64 {
    read_cf_u64(db, handle, rocksdb::properties::LIVE_SST_FILES_SIZE).unwrap_or(0)
}

/// Directories this process has open.
///
/// `fcntl` on `LOCK` is per process, so it does not stop a second open here.
/// The live handle compacts and deletes SST files that the second open has
/// already seen in the manifest. The keys are the canonical path and, when
/// that directory already exists, its `(st_dev, st_ino)`. A symlink or a
/// later name of that directory is the same open. A path whose last component
/// does not exist yet has no inode, so only the path is recorded. A clone
/// shares both counts. `mem::forget` leaves them, so an abandoned handle
/// still blocks that inode.
struct OpenDirectories {
    paths: HashMap<PathBuf, usize>,
    inodes: HashMap<(u64, u64), usize>,
}

fn open_directories() -> &'static Mutex<OpenDirectories> {
    static HELD: OnceLock<Mutex<OpenDirectories>> = OnceLock::new();
    HELD.get_or_init(|| {
        Mutex::new(OpenDirectories {
            paths: HashMap::new(),
            inodes: HashMap::new(),
        })
    })
}

struct OpenClaim {
    path: PathBuf,
    /// `Some((st_dev, st_ino))` when the directory existed at claim time.
    inode: Option<(u64, u64)>,
}

impl Clone for OpenClaim {
    fn clone(&self) -> Self {
        let mut held = open_directories()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.path.clone();
        *held.paths.entry(path.clone()).or_insert(0) += 1;
        if let Some(inode) = self.inode {
            *held.inodes.entry(inode).or_insert(0) += 1;
        }
        Self {
            path,
            inode: self.inode,
        }
    }
}

impl Drop for OpenClaim {
    fn drop(&mut self) {
        let mut held = open_directories()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = held.paths.get_mut(&self.path) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.paths.remove(&self.path);
            }
        }
        let Some(inode) = self.inode else {
            return;
        };
        let Some(count) = held.inodes.get_mut(&inode) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            held.inodes.remove(&inode);
        }
    }
}

fn claim_open_directory(path: &Path) -> eyre::Result<OpenClaim> {
    let key = open_directory_key(path)?;
    let mut held = open_directories()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Stat under the same lock as the insert. A symlink or a renamed
    // directory is this inode even when its canonical path differs.
    let inode = directory_inode(&key)?;
    let path_held = held.paths.get(&key).copied().unwrap_or(0) > 0;
    let inode_held = inode.is_some_and(|id| held.inodes.get(&id).copied().unwrap_or(0) > 0);
    if path_held || inode_held {
        eyre::bail!(
            "rocksdb directory {} is already open in this process. Close that handle before opening it again.",
            path.display()
        );
    }
    held.paths.insert(key.clone(), 1);
    if let Some(id) = inode {
        held.inodes.insert(id, 1);
    }
    Ok(OpenClaim { path: key, inode })
}

#[cfg(test)]
thread_local! {
    static CLAIM_DIRECTORY_STATS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Stats of the candidate directory. One claim stats that path once.
/// It does not stat the other directories this process has open.
#[cfg(test)]
fn claim_directory_stats() -> u64 {
    CLAIM_DIRECTORY_STATS.with(std::cell::Cell::get)
}

/// `(st_dev, st_ino)` when `path` is a directory. `None` when the last
/// component does not exist yet: the parent inode is not the new directory.
fn directory_inode(path: &Path) -> eyre::Result<Option<(u64, u64)>> {
    #[cfg(test)]
    CLAIM_DIRECTORY_STATS.with(|cell| cell.set(cell.get().saturating_add(1)));
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some((meta.dev(), meta.ino()))),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).wrap_err("stat rocksdb directory"),
    }
}

fn open_directory_key(path: &Path) -> eyre::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).wrap_err("canonicalize rocksdb directory");
    }
    let name = path
        .file_name()
        .ok_or_else(|| eyre::eyre!("rocksdb path has no directory name"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).wrap_err("canonicalize rocksdb parent")?;
    Ok(parent.join(name))
}

#[derive(Clone)]
pub struct RocksSubmoduleStore {
    /// Dropped first so the delay thread joins before `db` closes.
    lifetime: Arc<EngineLifetime>,
    /// Joins next so a CompactRange finishes before `db` closes.
    compact: Arc<SmallFileCompactor>,
    db: Arc<DB>,
    /// Dropped after `db`. The directory stays claimed until close finishes
    /// deleting SST files. A second open in this process is refused until then.
    open_claim: OpenClaim,
    /// Process cache and write-buffer manager when this open uses them.
    /// Declared after `db` so the database closes before this `Arc` drops.
    resources: Option<Arc<RocksResources>>,
    write: Arc<Mutex<()>>,
    group: Arc<GroupCommit>,
    path: PathBuf,
    /// Statistics object from the `Options` that opened this DB.
    ///
    /// The DB copies that shared pointer. Ticker reads have to use this
    /// object. Production open keeps it. Dump and persist periods are zero:
    /// a periodic stats write would show up as disk writes during a read sample.
    stats: Option<Arc<Options>>,
    /// Submodule directory name. Metric label. Not an absolute path.
    submodule: String,
    /// Chosen from the SCHEMA file before `DB::open`. An empty directory is RocksDB v1.
    layout: IndexLayout,
    /// A read-only open refuses every write, including compaction and the schema stamp.
    read_only: bool,
    /// Conversion open. Writes skip the WAL. [`Self::seal_and_stamp`] flushes
    /// the memtables, drops the log files, and writes the SCHEMA file last.
    bulk_load: bool,
    /// Exclusive `flock` of the RocksDB `LOCK` file. Drop releases it.
    ///
    /// Read-only open flocks `LOCK` when that file already exists. Conversion
    /// keeps the fd from before `CONVERTING` is visible, or flocks `LOCK`
    /// after `DB::open` when the caller did not pass one. Seal compares that
    /// fd with the path. Closing the fd releases this process's `fcntl` locks
    /// on `LOCK`, including RocksDB's, so the fd stays open until the database
    /// has closed.
    lock_file: Option<Arc<File>>,
    /// Runs after this database is closed and before the SCHEMA file.
    #[cfg(any(test, feature = "test-utils"))]
    before_schema: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Background work visible around one bench read shape.
///
/// Compaction and flush counters do not need statistics. `no_file_opens` is
/// cumulative since open and is present only when the bench enabled tickers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RocksBackground {
    pub compactions: u64,
    pub flushes: u64,
    pub compaction_pending: u64,
    pub flush_pending: u64,
    pub no_file_opens: Option<u64>,
}

impl std::fmt::Debug for RocksSubmoduleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksSubmoduleStore")
            .field("path", &self.path)
            .finish()
    }
}

impl RocksSubmoduleStore {
    pub fn open(path: impl AsRef<Path>) -> eyre::Result<Self> {
        Self::open_inner(path, RocksTuning::baseline(), true, false, None, true, None)
    }

    /// Same open as [`Self::open`], with a private block cache.
    ///
    /// The bench uses this to repeat a read test at more than one cache size.
    /// The private cache opts out of the process set, so `block_cache_bytes`
    /// is the cache that opens. Production [`Self::open`] uses the shared set.
    pub fn open_with_block_cache(
        path: impl AsRef<Path>,
        block_cache_bytes: usize,
    ) -> eyre::Result<Self> {
        Self::open_with(
            path,
            RocksTuning::baseline().with_block_cache(block_cache_bytes),
        )
    }

    /// Open with an explicit preset. [`Self::open`] stays on the baseline.
    pub fn open_with(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Self::open_inner(path, tuning, false, false, None, true, None)
    }

    /// Bench open. Same table cache as [`Self::open`], and it skips the
    /// table-property scan during `DB::Open`.
    ///
    /// Histograms and timers stay off. An empty directory is RocksDB v1.
    pub fn open_with_stats(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Self::open_inner(path, tuning, true, true, None, true, None)
    }

    /// Bench open. Same cache and table scan as [`Self::open_with_stats`].
    ///
    /// An empty directory is RocksDB v1. A schema file that is not that
    /// marker is refused.
    pub fn open_bench(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Self::open_inner(
            path,
            tuning,
            true,
            true,
            None,
            true,
            Some(IndexLayout::RocksV1),
        )
    }

    /// Baseline open on an explicit resource set.
    ///
    /// The set wins over the process `OnceLock`. Tests use this to measure
    /// one cache without depending on whatever this process installed first.
    pub fn open_with_resources(
        path: impl AsRef<Path>,
        resources: Arc<RocksResources>,
    ) -> eyre::Result<Self> {
        Self::open_inner(
            path,
            RocksTuning::baseline(),
            true,
            false,
            Some(resources),
            true,
            None,
        )
    }

    /// `true` when this database uses a shared cache and write-buffer manager.
    pub fn uses_shared_resources(&self) -> bool {
        self.resources.is_some()
    }

    /// Opens a RocksDB v1 conversion target. Does not write the SCHEMA file.
    ///
    /// Writes skip the WAL (`recycle_log_file_num` is 0; a non-zero count
    /// rejects `disableWAL`). [`Self::seal_and_stamp`] makes the rows durable
    /// and writes the RocksDB v1 marker last. A crash before that leaves
    /// `CURRENT` and no SCHEMA file.
    ///
    /// Flocks `LOCK` after `DB::open` and holds that fd until seal.
    ///
    /// Tests use this after dropping the flock returned by prepare. Production
    /// passes that flock to [`Self::open_converting_with_lock`].
    #[cfg(test)]
    pub(super) fn open_converting(path: impl AsRef<Path>) -> eyre::Result<Self> {
        Self::open_converting_with_lock(path, None)
    }

    /// Opens a conversion target, optionally with a flock already held on `LOCK`.
    ///
    /// `lock` must be the `LOCK` file at `path`. `None` flocks `LOCK` after
    /// `DB::open`. A different inode means the directory was replaced. This
    /// function does not flock a second time: a second `flock` in this process
    /// fails, and closing either fd would drop RocksDB's `fcntl` lock.
    pub(super) fn open_converting_with_lock(
        path: impl AsRef<Path>,
        lock: Option<Arc<File>>,
    ) -> eyre::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        if let Some(file) = &lock
            && !conversion_lock_protects(file, &path_buf)
        {
            eyre::bail!(
                "rocksdb directory {} was replaced. Refusing to open it.",
                path_buf.display()
            );
        }
        let mut tuning = RocksTuning::baseline();
        tuning.recycle_log_file_num = 0;
        let mut opened = Self::open_inner(
            &path_buf,
            tuning,
            true,
            false,
            None,
            false,
            Some(IndexLayout::RocksV1),
        )?;
        opened.bulk_load = true;
        opened.lock_file = Some(match lock {
            Some(file) => file,
            None => acquire_conversion_lock(&path_buf)?,
        });
        Ok(opened)
    }

    /// Read-only open of a completed index. Does not create families or write the schema.
    ///
    /// Flocks `LOCK` when that file exists and holds it until this store drops.
    /// A missing `LOCK` means nothing holds it; this function does not create one.
    pub(super) fn open_read_only(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        refuse_unresolved_close_scratch(&path)?;
        if !path.join(SCHEMA_FILE).is_file() {
            eyre::bail!(
                "rocks index {} has no SCHEMA marker. Refusing a read-only open of an incomplete directory.",
                path.display()
            );
        }
        // Read-only open does not create a database. Refuse before RocksDB
        // reports a missing CURRENT, so the error names the restore.
        refuse_schema_without_current(&path)?;
        let open_claim = claim_open_directory(&path)?;
        let lock_file = hold_rocks_lock(&path)?;
        let layout = resolve_layout(&path, None)?;
        let unknown = existing_unknown_families(&path)?;
        log_unknown_families(&path, &unknown);
        let db = open_db_read_only(&path)?;
        // Absent flags are zero. Read-only open does not stamp them.
        ensure_feature_flags(&db, &path, false, false)?;
        let group = GroupCommit::new();
        let db = Arc::new(db);
        let submodule = submodule_label(&path);
        Ok(Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            compact: SmallFileCompactor::start(Arc::clone(&db), 0, submodule.clone())?,
            db,
            open_claim,
            resources: None,
            write: Arc::new(Mutex::new(())),
            group,
            submodule,
            path,
            stats: None,
            layout,
            read_only: true,
            bulk_load: false,
            lock_file,
            #[cfg(any(test, feature = "test-utils"))]
            before_schema: None,
        })
    }

    /// SST and blob files per column family. Blob counts use live files when
    /// RocksDB lists them, and the blob-file properties otherwise.
    pub fn column_file_usage(&self) -> eyre::Result<Vec<ColumnFileUsage>> {
        let mut by_name: BTreeMap<String, ColumnFileUsage> = BTreeMap::new();
        for file in self.db.live_files().wrap_err("list live files")? {
            let entry = by_name
                .entry(file.column_family_name.clone())
                .or_insert_with(|| ColumnFileUsage {
                    name: file.column_family_name.clone(),
                    sst_files: 0,
                    sst_bytes: 0,
                    blob_files: 0,
                    blob_bytes: 0,
                });
            let bytes = u64::try_from(file.size).unwrap_or(u64::MAX);
            if file.name.ends_with(".blob") {
                entry.blob_files = entry.blob_files.saturating_add(1);
                entry.blob_bytes = entry.blob_bytes.saturating_add(bytes);
            } else {
                entry.sst_files = entry.sst_files.saturating_add(1);
                entry.sst_bytes = entry.sst_bytes.saturating_add(bytes);
            }
        }
        let names = DB::list_cf(&Options::default(), &self.path).unwrap_or_default();
        for name in names {
            let Some(handle) = self.db.cf_handle(&name) else {
                continue;
            };
            let entry = by_name
                .entry(name.clone())
                .or_insert_with(|| ColumnFileUsage {
                    name: name.clone(),
                    sst_files: 0,
                    sst_bytes: 0,
                    blob_files: 0,
                    blob_bytes: 0,
                });
            if entry.blob_files == 0 {
                entry.blob_files = cf_property_u64(&self.db, handle, "rocksdb.num-blob-files");
                entry.blob_bytes =
                    cf_property_u64(&self.db, handle, "rocksdb.total-blob-file-size");
            }
        }
        Ok(by_name.into_values().collect())
    }

    /// Replace offset-family values with these bytes. No codec.
    ///
    /// RocksDB v1 writes `ChunkMetaByOffset`.
    ///
    /// The bench uses this to compare record layouts. Production writes go
    /// through [`SubmoduleWrite::write_packed_offset_rows`]. The WAL stays
    /// on: this open recycles log files, and that option rejects
    /// `disableWAL`. The caller compacts before it reads SST sizes, so the
    /// WAL is not part of the measurement.
    pub fn write_raw_data_paths(
        &self,
        rows: impl IntoIterator<Item = (PartitionChunkOffset, Vec<u8>)>,
    ) -> eyre::Result<()> {
        let _guard = self.lock_write();
        let handle = cf_handle(&self.db, Cf::OffsetMeta)?;
        let mut batch = WriteBatch::default();
        let mut pending = 0_u32;
        for (offset, value) in rows {
            batch.put_cf(&handle, encode_key(offset), value);
            pending += 1;
            if pending == 4_096 {
                self.db
                    .write_opt(std::mem::take(&mut batch), &write_opts(false, false))
                    .wrap_err("write raw offset rows")?;
                pending = 0;
            }
        }
        if pending > 0 {
            self.db
                .write_opt(batch, &write_opts(false, false))
                .wrap_err("write raw offset rows")?;
        }
        Ok(())
    }

    /// Flush and compact the offset family, rewriting the bottom level.
    pub fn compact_data_paths(&self) -> eyre::Result<()> {
        let _guard = self.lock_write();
        let handle = cf_handle(&self.db, Cf::OffsetMeta)?;
        self.db
            .flush_cf(&handle)
            .wrap_err("flush data path column")?;
        let mut opts = rocksdb::CompactOptions::default();
        opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
        opts.set_exclusive_manual_compaction(true);
        self.db
            .compact_range_cf_opt(&handle, None::<&[u8]>, None::<&[u8]>, &opts);
        Ok(())
    }

    fn open_inner(
        path: impl AsRef<Path>,
        tuning: RocksTuning,
        collect_stats: bool,
        preload_tables: bool,
        resources: Option<Arc<RocksResources>>,
        stamp_schema: bool,
        force: Option<IndexLayout>,
    ) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        refuse_unresolved_close_scratch(&path)?;
        let open_claim = claim_open_directory(&path)?;
        let layout = if stamp_schema {
            // CURRENT without SCHEMA, or any other non-empty directory without
            // SCHEMA, is not a new database. Stamping it would hide a crash.
            refuse_incomplete_directory(&path)?;
            resolve_layout(&path, force)?
        } else {
            if path.join(SCHEMA_FILE).is_file() {
                eyre::bail!(
                    "refusing to convert into completed rocks index {}",
                    path.display()
                );
            }
            force.unwrap_or(IndexLayout::RocksV1)
        };
        let resources = shared_resources_for(tuning, resources)?;
        // An existing database (CURRENT is present) must already contain
        // every RocksDB v1 family. A missing one is not created empty.
        // A directory with no CURRENT is new, including a converter target,
        // and that open creates the families.
        let unknown = existing_unknown_families(&path)?;
        log_unknown_families(&path, &unknown);
        // Recovery flushes a non-empty memtable with the options of this
        // open. Default options drop a prefix extractor. Refuse before open.
        refuse_unknown_prefix_extractor(&path, &unknown)?;
        // That flush retires the WAL before the feature flags and the schema
        // row are checked, so a refusal keeps the new SST and manifest.
        // Read both through a read-only open: it applies the log in memory
        // and does not write it. A missing row is not stamped on that open.
        refuse_unwritable_recovery(&path)?;
        let create_missing = !path.join("CURRENT").is_file();
        let (db, stats) = open_db(
            &path,
            &tuning,
            collect_stats,
            preload_tables,
            resources.as_deref(),
            create_missing,
            &unknown,
        )?;
        if stamp_schema {
            ensure_feature_flags(&db, &path, true, false)?;
            ensure_schema_row(&db)?;
            let marker = path.join(SCHEMA_FILE);
            if !marker.exists() {
                write_schema_marker_file(&path, SUBMODULE_SCHEMA_ROCKS_V1)?;
            }
        }
        let group = GroupCommit::new();
        let db = Arc::new(db);
        let submodule = submodule_label(&path);
        Ok(Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            compact: SmallFileCompactor::start(
                Arc::clone(&db),
                tuning.small_file_compact_threshold,
                submodule.clone(),
            )?,
            db,
            open_claim,
            resources,
            write: Arc::new(Mutex::new(())),
            group,
            submodule,
            path,
            stats,
            layout,
            read_only: false,
            bulk_load: false,
            lock_file: None,
            #[cfg(any(test, feature = "test-utils"))]
            before_schema: None,
        })
    }

    /// Observe the window after close and before the SCHEMA file.
    #[cfg(test)]
    pub(super) fn set_before_schema_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.before_schema = Some(Arc::new(hook));
    }

    /// Flush copied rows into SST files, drop WAL logs, then write SCHEMA.
    ///
    /// The database is closed before the log files are removed. `flock` on
    /// `LOCK` is taken before that close and dropped only after SCHEMA is
    /// synced. RocksDB's own lock is `fcntl`, which does not block `flock`,
    /// so this can be taken while the database is still open. A crash before
    /// the file leaves no completion marker, and the flock dies with the
    /// process, so the run is redone. A family with more than one SST is
    /// compacted so the finished directory is not a pile of flushes.
    ///
    /// SCHEMA is written only when the held `LOCK` fd is still the file at
    /// this path. A missing file, or a different inode, means the directory
    /// was replaced. `LOCK` on the new directory is not this database.
    pub(super) fn seal_and_stamp(mut self) -> eyre::Result<()> {
        let path = self.path.clone();
        #[cfg(any(test, feature = "test-utils"))]
        let before_schema = self.before_schema.clone();
        // Held across flush, close, WAL removal, and the SCHEMA write. A
        // second process takes this flock before it deletes a CONVERTING
        // directory. Prefer the fd captured at open. A new flock would attach
        // to whatever `LOCK` is at the path now. Flush and the schema row
        // write files by that path, so a replaced directory is refused first.
        let flock = match self.lock_file.take() {
            Some(file) => file,
            None => match hold_rocks_flock(&path)? {
                Some(file) => Arc::new(file),
                None => {
                    close_without_touching_directory(self, &path)?;
                    eyre::bail!(
                        "rocksdb LOCK is missing in {}. Refusing to write SCHEMA.",
                        path.display()
                    );
                }
            },
        };
        // Queued flush and compaction must finish in this directory before a
        // later check can refuse the seal. Both refuse arms abandon the handle.
        // They do not flush, and they do not cancel background work: cancel
        // flushes when the WAL is off. A flush already running when the
        // directory is renamed still finishes. Waiting after that rename
        // writes the SST files into the replacement.
        if !drain_while_lock_protects(&self, &flock, &path)? {
            close_without_touching_directory(self, &path)?;
            eyre::bail!(
                "rocksdb directory {} was replaced. Refusing to write SCHEMA.",
                path.display()
            );
        }
        {
            self.ensure_writable()?;
            let _guard = self.lock_write();
            self.flush_families_locked()?;
            self.compact_split_families_locked();
            write_schema_row(&self.db, true)?;
            ensure_feature_flags(&self.db, &path, true, true)?;
            self.flush_families_locked()?;
            self.compact_split_families_locked();
        }
        // Compaction from the flush above. Skip the wait when the directory
        // was already replaced: waiting would finish that publish.
        if !drain_while_lock_protects(&self, &flock, &path)? {
            close_without_touching_directory(self, &path)?;
            eyre::bail!(
                "rocksdb directory {} was replaced. Refusing to write SCHEMA.",
                path.display()
            );
        }
        drop(self);
        if !conversion_lock_protects(&flock, &path) {
            eyre::bail!(
                "rocksdb directory {} was replaced. Refusing to write SCHEMA.",
                path.display()
            );
        }
        remove_rocks_wal_files(&path)?;
        sync_dir(&path)?;
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(hook) = before_schema {
            hook();
        }
        if !conversion_lock_protects(&flock, &path) {
            eyre::bail!(
                "rocksdb directory {} was replaced. Refusing to write SCHEMA.",
                path.display()
            );
        }
        write_schema_marker_file(&path, SUBMODULE_SCHEMA_ROCKS_V1)
    }
}

/// Wait for flush and compaction that are already queued.
///
/// Does not start a flush. A new flush writes SST files through `dbname_`.
fn drain_background_work(store: &RocksSubmoduleStore) -> eyre::Result<()> {
    #[cfg(test)]
    BACKGROUND_DRAINS.with(|cell| cell.set(cell.get().saturating_add(1)));
    let mut opts = rocksdb::WaitForCompactOptions::default();
    // Leave `flush` false. A new flush writes SST files through dbname_.
    opts.set_flush(false);
    store
        .db
        .wait_for_compact(&opts)
        .wrap_err("drain rocksdb background flush and compaction")
}

/// `true` when the wait finished and `flock` still names `path`.
///
/// `false` when the directory was already replaced, or the wait failed after
/// that replacement. The caller abandons without flushing and without cancel.
fn drain_while_lock_protects(
    store: &RocksSubmoduleStore,
    flock: &File,
    path: &Path,
) -> eyre::Result<bool> {
    if !conversion_lock_protects(flock, path) {
        return Ok(false);
    }
    if let Err(err) = drain_background_work(store) {
        // `Err` makes the caller drop `store`, and that drop flushes. A
        // replaced directory takes the abandon path instead.
        if !conversion_lock_protects(flock, path) {
            return Ok(false);
        }
        return Err(err);
    }
    #[cfg(test)]
    run_after_protected_drain();
    Ok(conversion_lock_protects(flock, path))
}

/// Put the directory at `path` back without closing `store`.
///
/// `rocksdb_close` flushes through the still-open manifest fd. That fd is the
/// directory moved aside, while the new SST files are created in the empty
/// directory at `path`. Deleting the empty directory then drops those SST
/// files and leaves the moved manifest naming them. Leak the handle instead.
///
/// A sibling marker names the parked directory before the rename. `remove_dir_all`
/// of `path` does not remove that marker. Open and replace refuse while it exists.
/// The marker is removed only after the parked directory is restored.
fn close_without_touching_directory(store: RocksSubmoduleStore, path: &Path) -> eyre::Result<()> {
    let Some(parent) = path.parent() else {
        abandon_open_database(store);
        return Ok(());
    };
    if !path.exists() {
        // Creating the path and closing into it publishes CURRENT into a
        // directory this database does not own. A failed delete would leave
        // that publish in place.
        abandon_open_database(store);
        eyre::bail!(
            "rocksdb directory {} is missing. Refusing to create it for an abandoned close.",
            path.display()
        );
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let parked = parent.join(format!(
        ".{name}.close-scratch-{}-{}",
        std::process::id(),
        CLOSE_SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let scratch_name = parked
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    // A `?` here drops `store` while `path` is still the caller's directory.
    // That close flushes SST files, a new manifest, and CURRENT into it.
    if let Err(err) = write_close_scratch_marker(path, &scratch_name) {
        abandon_open_database(store);
        return Err(err);
    }
    #[cfg(test)]
    if close_scratch_stopped(CloseScratchStop::Marker) {
        abandon_open_database(store);
        eyre::bail!("stopped refused close");
    }
    if let Err(err) = fs::rename(path, &parked) {
        let _ = remove_close_scratch_marker(path);
        abandon_open_database(store);
        return Err(err).wrap_err_with(|| {
            format!("park {} before an abandoned rocksdb close", path.display())
        });
    }
    #[cfg(test)]
    if close_scratch_stopped(CloseScratchStop::Rename) {
        abandon_open_database(store);
        eyre::bail!("stopped refused close");
    }
    if let Err(err) = fs::create_dir(path) {
        let restored = fs::rename(&parked, path);
        if restored.is_ok() {
            let _ = remove_close_scratch_marker(path);
        }
        abandon_open_database(store);
        restored.wrap_err_with(|| {
            format!(
                "restore {} after a failed scratch create; the directory is at {}",
                path.display(),
                parked.display()
            )
        })?;
        return Err(err).wrap_err("create directory for abandoned rocksdb close");
    }
    #[cfg(test)]
    if close_scratch_stopped(CloseScratchStop::EmptyCreate) {
        abandon_open_database(store);
        eyre::bail!("stopped refused close");
    }
    // Do not drop `store` here. Drop runs `rocksdb_close`, which records the
    // shutdown flush in the parked manifest and writes the SST files into this
    // empty directory. The delete below would remove those files.
    abandon_open_database(store);
    #[cfg(test)]
    if close_scratch_stopped(CloseScratchStop::Close) {
        eyre::bail!("stopped refused close");
    }
    let removed = fs::remove_dir_all(path);
    if let Err(err) = fs::rename(&parked, path) {
        return Err(err).wrap_err_with(|| {
            format!(
                "restore {} after abandoned rocksdb close; the directory is at {}",
                path.display(),
                parked.display()
            )
        });
    }
    #[cfg(test)]
    if close_scratch_stopped(CloseScratchStop::Restore) {
        removed?;
        eyre::bail!("stopped refused close");
    }
    remove_close_scratch_marker(path)?;
    removed.wrap_err("remove abandoned rocksdb close directory")
}

fn close_scratch_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn close_scratch_marker_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    close_scratch_parent(path).join(format!(".{name}.close-scratch-marker"))
}

/// Durable sibling that names the parked directory.
///
/// The file sits in the parent, so deleting or renaming `path` leaves it.
fn write_close_scratch_marker(path: &Path, scratch_name: &str) -> eyre::Result<()> {
    let marker = close_scratch_marker_path(path);
    let tmp = PathBuf::from(format!("{}.tmp", marker.display()));
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .wrap_err("create close scratch marker")?;
        file.write_all(scratch_name.as_bytes())
            .wrap_err("write close scratch marker")?;
        file.write_all(b"\n")
            .wrap_err("write close scratch marker")?;
        file.sync_all().wrap_err("sync close scratch marker")?;
    }
    fs::rename(&tmp, &marker).wrap_err("rename close scratch marker")?;
    sync_dir(close_scratch_parent(path))
}

fn remove_close_scratch_marker(path: &Path) -> eyre::Result<()> {
    let marker = close_scratch_marker_path(path);
    match fs::remove_file(&marker) {
        Ok(()) => sync_dir(close_scratch_parent(path)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).wrap_err("remove close scratch marker"),
    }
}

/// Refuse open and replace while a refused close still has a parked directory.
///
/// The marker names the scratch. A directory whose name is that scratch is
/// the parked index, so opening it is refused as well.
pub(super) fn refuse_unresolved_close_scratch(path: &Path) -> eyre::Result<()> {
    let parent = close_scratch_parent(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let marker = parent.join(format!(".{name}.close-scratch-marker"));
    if marker.is_file() {
        let named = fs::read_to_string(&marker).unwrap_or_default();
        let named = named.trim();
        let scratch = if named.is_empty() {
            marker.display().to_string()
        } else {
            parent.join(named).display().to_string()
        };
        eyre::bail!(
            "rocksdb directory {} has an unresolved close scratch {scratch}. The index bytes are in that sibling, or still in this directory when the sibling is absent. Refusing to open or replace this directory until the scratch is restored.",
            path.display()
        );
    }
    if !parent.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(parent).wrap_err("read parent for a close scratch marker")? {
        let entry = entry.wrap_err("read parent for a close scratch marker")?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.ends_with(".close-scratch-marker") {
            continue;
        }
        let text = fs::read_to_string(entry.path()).unwrap_or_default();
        if text.trim() == name {
            eyre::bail!(
                "rocksdb directory {} is the close scratch {} named by {}. Refusing to open or replace it.",
                path.display(),
                path.display(),
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn abandon_open_database(store: RocksSubmoduleStore) {
    let RocksSubmoduleStore {
        lifetime,
        compact,
        db,
        open_claim,
        resources,
        stats,
        write,
        group,
        lock_file,
        ..
    } = store;
    // Join threads that call into the database, then leak the handle. Closing
    // it would write through the path this process no longer owns. Keep the
    // claim: the leaked handle can still compact that directory.
    drop(lifetime);
    drop(compact);
    drop(write);
    drop(group);
    std::mem::forget(db);
    std::mem::forget(open_claim);
    std::mem::forget(resources);
    std::mem::forget(stats);
    std::mem::forget(lock_file);
}

static CLOSE_SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseScratchStop {
    Marker,
    Rename,
    EmptyCreate,
    Close,
    Restore,
}

#[cfg(test)]
thread_local! {
    static CLOSE_SCRATCH_STOP: std::cell::Cell<Option<CloseScratchStop>> =
        const { std::cell::Cell::new(None) };
    static AFTER_PROTECTED_DRAIN: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    static BACKGROUND_DRAINS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct AfterProtectedDrainGuard;

#[cfg(test)]
impl Drop for AfterProtectedDrainGuard {
    fn drop(&mut self) {
        AFTER_PROTECTED_DRAIN.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Runs after a protected drain check and before the lock is read again.
/// The directory is still the one the handle opened, unless the hook renames it.
#[cfg(test)]
fn set_after_protected_drain(hook: impl FnMut() + 'static) -> AfterProtectedDrainGuard {
    AFTER_PROTECTED_DRAIN.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
    AfterProtectedDrainGuard
}

#[cfg(test)]
fn run_after_protected_drain() {
    AFTER_PROTECTED_DRAIN.with(|cell| {
        if let Some(hook) = cell.borrow_mut().as_mut() {
            hook();
        }
    });
}

#[cfg(test)]
fn reset_background_drains() {
    BACKGROUND_DRAINS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn background_drains() -> u64 {
    BACKGROUND_DRAINS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(super) struct CloseScratchStopGuard;

#[cfg(test)]
impl Drop for CloseScratchStopGuard {
    fn drop(&mut self) {
        CLOSE_SCRATCH_STOP.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
pub(super) fn set_close_scratch_stop(stop: CloseScratchStop) -> CloseScratchStopGuard {
    CLOSE_SCRATCH_STOP.with(|cell| cell.set(Some(stop)));
    CloseScratchStopGuard
}

#[cfg(test)]
fn close_scratch_stopped(stage: CloseScratchStop) -> bool {
    CLOSE_SCRATCH_STOP.with(|cell| cell.get() == Some(stage))
}

impl RocksSubmoduleStore {
    fn flush_families_locked(&self) -> eyre::Result<()> {
        for cf in families() {
            let handle = cf_handle(&self.db, cf)?;
            self.db.flush_cf(&handle).wrap_err("flush column family")?;
        }
        Ok(())
    }

    /// Merge a family that flushed more than one SST. One file is left as it is.
    fn compact_split_families_locked(&self) {
        for cf in families() {
            let Ok(handle) = cf_handle(&self.db, cf) else {
                continue;
            };
            if live_sst_count(&self.db, handle) <= 1 {
                continue;
            }
            let mut opts = rocksdb::CompactOptions::default();
            opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
            opts.set_exclusive_manual_compaction(true);
            self.db
                .compact_range_cf_opt(&handle, None::<&[u8]>, None::<&[u8]>, &opts);
        }
    }

    pub(super) fn schema_bytes(&self) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let key = encode_key(MetadataKey::DBSchemaVersion);
        match self.db.get_cf(&handle, key).wrap_err("read schema row")? {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(None),
        }
    }

    /// Next keys after `after`. Values are not read.
    pub(super) fn read_cf_keys<K>(
        &self,
        cf: &str,
        after: Option<K>,
        limit: usize,
    ) -> eyre::Result<Vec<K>>
    where
        K: Decode + Encode + Ord + Copy,
    {
        let limit = limit.max(1);
        let handle = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| eyre::eyre!("missing column family {cf}"))?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        // A prefix extractor stops `Next` unless the seek is total order.
        opts.set_total_order_seek(true);
        let encoded = after.map(encode_key);
        let snap = self.db.snapshot();
        let mut iter = snap.raw_iterator_cf_opt(handle, opts);
        match &encoded {
            None => iter.seek_to_first(),
            Some(bytes) => iter.seek(bytes),
        }
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(key_bytes) = iter.key() else {
                break;
            };
            let key =
                K::decode(key_bytes).map_err(|err| eyre::eyre!("decode key in {cf}: {err}"))?;
            iter.next();
            if after.is_some_and(|prev| key <= prev) {
                continue;
            }
            out.push(key);
        }
        iter.status().wrap_err("scan rocksdb keys")?;
        Ok(out)
    }

    /// On-disk column family names, including `default`.
    pub(super) fn column_family_names(&self) -> eyre::Result<Vec<String>> {
        DB::list_cf(&Options::default(), &self.path).wrap_err("list column families")
    }

    /// Next raw key/value pairs after `after`, in total order.
    ///
    /// A missing family is an empty page. Values are the stored bytes.
    pub(super) fn read_raw_cf_page(
        &self,
        name: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> eyre::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let limit = limit.max(1);
        let Some(handle) = self.db.cf_handle(name) else {
            return Ok(Vec::new());
        };
        let snap = self.db.snapshot();
        let mut iter = snap.raw_iterator_cf_opt(handle, full_scan_options());
        match after {
            None => iter.seek_to_first(),
            Some(key) => iter.seek(key),
        }
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(key) = iter.key() else {
                break;
            };
            if after.is_some_and(|prev| key <= prev) {
                iter.next();
                continue;
            }
            let owned = key.to_vec();
            let value = iter.value().unwrap_or(&[]).to_vec();
            out.push((owned, value));
            iter.next();
        }
        iter.status()
            .wrap_err_with(|| format!("scan column family {name}"))?;
        Ok(out)
    }

    /// Next offset-row keys. Values are not decoded.
    ///
    /// RocksDB v1 reads `ChunkMetaByOffset`.
    pub(super) fn read_offset_row_keys(
        &self,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        self.read_cf_keys(self.offset_family_name(), after, limit)
    }

    /// On-disk name of the offset family.
    fn offset_family_name(&self) -> &'static str {
        CHUNK_META_BY_OFFSET_CF
    }

    /// Next pending-job keys in this layout. Values are not decoded.
    pub(super) fn read_pending_keys(
        &self,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        self.read_cf_keys(Cf::Pending.name(), after, limit)
    }

    /// Next RocksDB v1 placements after `after`, in start-offset order.
    pub(super) fn read_placements(
        &self,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<Placement>> {
        eyre::ensure!(
            self.layout == IndexLayout::RocksV1,
            "placement scan requires a RocksDB v1 index"
        );
        let limit = limit.max(1);
        let handle = cf_handle(&self.db, Cf::Interval)?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        let encoded = after.map(encode_key);
        let mode = match &encoded {
            None => IteratorMode::Start,
            Some(bytes) => IteratorMode::From(bytes.as_slice(), Direction::Forward),
        };
        let snap = self.db.snapshot();
        let mut out = Vec::new();
        for item in snap.iterator_cf_opt(&handle, opts, mode) {
            let (key, value) = item.wrap_err("scan placement interval")?;
            let start = PartitionChunkOffset::decode(key.as_ref())
                .wrap_err("placement interval key is not an offset")?
                .0;
            if after.is_some_and(|prev| start <= prev.0) {
                continue;
            }
            let decoded = records::decode_placement(value.as_ref())?;
            out.push(Placement {
                data_root: H256(decoded.data_root),
                tx_start_offset: irys_types::RelativeChunkOffset(decoded.tx_start),
                data_size: decoded.data_size,
                ledger: records::optional_u32(decoded.ledger),
                start: PartitionChunkOffset(start),
                end: PartitionChunkOffset(decoded.end),
                block_height: records::optional_u64(decoded.block_height),
                tx_index: records::optional_u32(decoded.tx_index),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// End offset of the last placement. `None` when this index has no placement.
    pub(super) fn highest_placement_end(&self) -> eyre::Result<Option<PartitionChunkOffset>> {
        let handle = cf_handle(&self.db, Cf::Interval)?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        let snap = self.db.snapshot();
        let mut iter = snap.iterator_cf_opt(&handle, opts, IteratorMode::End);
        let Some(item) = iter.next() else {
            return Ok(None);
        };
        let (key, value) = item.wrap_err("read the last placement")?;
        PartitionChunkOffset::decode(key.as_ref())
            .wrap_err("placement interval key is not an offset")?;
        let end = records::decode_placement(value.as_ref())?.end;
        Ok(Some(PartitionChunkOffset(end)))
    }

    pub(super) fn enable_group_commit(&self, txs_per_sync: u32) -> eyre::Result<()> {
        // A caller that already set the bound keeps it. Otherwise one second.
        if !self.group.delay_is_set() {
            self.group.set_delay(Duration::from_millis(
                irys_types::DEFAULT_INDEX_WAL_BACKSTOP_MS,
            ));
        }
        match self.group.prepare(txs_per_sync)? {
            EnableStep::On => Ok(()),
            EnableStep::Spawn => {
                if let Err(err) = self.spawn_delay() {
                    self.group.clear_spawned();
                    return Err(err);
                }
                self.group.finish_enable();
                Ok(())
            }
            EnableStep::Arm => {
                self.group.finish_enable();
                Ok(())
            }
        }
    }

    pub(super) fn group_commit_enabled(&self) -> bool {
        self.group.is_enabled()
    }

    #[cfg(test)]
    pub(super) fn group_sync_count(&self) -> u64 {
        self.group.sync_count()
    }

    pub(super) fn sync_group(&self) -> eyre::Result<()> {
        if !self.group.is_enabled() {
            return Ok(());
        }
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        sync_wal_locked(&self.db, &self.group, &self.submodule)
    }

    /// The next dirty WAL sync fails once. Later writes then fail.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn fail_next_wal_sync(&self) {
        self.group.arm_fail_next_sync();
    }

    /// The next synced write or compaction WAL flush fails once.
    /// An unsynced group append does not take this.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn fail_next_synced_write(&self) {
        self.group.arm_fail_next_synced_write();
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn wal_sync_failed(&self) -> bool {
        self.group.wal_failed()
    }

    pub(super) fn set_wal_backstop(&self, delay: Duration) {
        self.group.set_delay(delay);
    }

    pub(super) fn published_generation(&self) -> u64 {
        self.group.generation()
    }

    pub(super) fn durable_generation(&self) -> u64 {
        self.group.synced_generation()
    }

    pub(super) fn wait_until_durable(&self, generation: u64) -> eyre::Result<()> {
        self.group.wait_until_synced(generation)
    }

    pub(super) fn wal_sync_count(&self) -> u64 {
        self.group.wal_syncs()
    }

    pub(super) fn wal_backstop_sync_count(&self) -> u64 {
        self.group.backstop_syncs()
    }

    #[cfg(test)]
    pub(super) fn wal_backstop_delay(&self) -> Duration {
        self.group.delay()
    }

    /// Visible registration. The caller holds no write lock.
    ///
    /// With group commit on, the WAL record is not synced here. The next
    /// synced write, the backstop, or shutdown covers it. The count that
    /// fills an MDBX group does not sync this database.
    pub(super) fn update_registration<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        self.update_covered(f)
    }

    /// WAL append without a sync when group commit is on.
    ///
    /// [`GroupCommit::note_visible`] arms the backstop. The returned bool is
    /// the MDBX "group is full" signal and is ignored here. Group commit off
    /// is a normal synced [`Self::update`].
    pub(super) fn update_covered<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        if !self.group.is_enabled() {
            return self.update(f);
        }
        self.ensure_writable()?;
        let _guard = self.lock_write();
        // The sync that sets the flag holds this lock.
        self.ensure_writable()?;
        let result = self.apply(false, f)?;
        self.group.note_visible();
        Ok(result)
    }

    fn ensure_writable(&self) -> eyre::Result<()> {
        if self.read_only {
            eyre::bail!("submodule index {} is open read-only", self.path.display());
        }
        if self.group.wal_failed() {
            eyre::bail!(
                "submodule index {} is read-only after a WAL sync failure",
                self.submodule
            );
        }
        Ok(())
    }

    fn spawn_delay(&self) -> eyre::Result<()> {
        let db = Arc::downgrade(&self.db);
        let write = Arc::clone(&self.write);
        let group = Arc::clone(&self.group);
        let submodule = self.submodule.clone();
        self.lifetime
            .spawn(Arc::clone(&self.group), move || match db.upgrade() {
                Some(db) => {
                    let _guard = write
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sync_wal_locked(&db, &group, &submodule)
                }
                None => Ok(()),
            })
    }

    fn lock_write(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `sync` waits until the WAL is durable. The write lock is held.
    fn apply<R>(
        &self,
        sync: bool,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        let snap = self.db.snapshot();
        let mut batch = Batch {
            db: &self.db,
            snap: &snap,
            overlay: Overlay::default(),
            layout: self.layout,
            bindings: HashMap::new(),
            unfilled: Vec::new(),
            origin: None,
        };
        let result = f(&mut batch)?;
        // Conversion skips the WAL. Durability is the later memtable flush.
        let disable_wal = self.bulk_load;
        batch.commit(
            sync && !disable_wal,
            disable_wal,
            &self.submodule,
            &self.group,
        )?;
        if sync && !disable_wal {
            self.group.note_wal_sync();
        }
        Ok(result)
    }

    /// Compactions, flushes, and table opens. Safe to call during reads.
    pub fn background(&self) -> eyre::Result<RocksBackground> {
        Ok(RocksBackground {
            compactions: property_u64(&self.db, rocksdb::properties::NUM_RUNNING_COMPACTIONS)?,
            flushes: property_u64(&self.db, rocksdb::properties::NUM_RUNNING_FLUSHES)?,
            compaction_pending: property_u64(&self.db, rocksdb::properties::COMPACTION_PENDING)?,
            flush_pending: property_u64(&self.db, rocksdb::properties::MEM_TABLE_FLUSH_PENDING)?,
            no_file_opens: self
                .stats
                .as_ref()
                .map(|opts| opts.get_ticker_count(rocksdb::statistics::Ticker::NoFileOpens)),
        })
    }

    /// Gauges and ticker counters for one submodule database.
    ///
    /// Property reads do not take the write lock. Tickers are cumulative.
    /// The caller publishes them with `counter.absolute` so a 60 s scrape
    /// does not double-count. MDBX indexes do not call this.
    pub fn report_metrics(&self) {
        use rocksdb::statistics::Ticker::{
            BlockCacheDataHit, BlockCacheDataMiss, BlockCacheFilterAdd,
            BlockCacheFilterBytesInsert, BlockCacheFilterHit, BlockCacheFilterMiss,
            BlockCacheIndexHit, BlockCacheIndexMiss, BytesRead, CompactReadBytes,
            CompactWriteBytes, FlushWriteBytes, NoFileOpens, NumberKeysRead, StallMicros,
            WalFileBytes, WalFileSynced,
        };
        describe_rocks_metrics();
        let submodule = self.submodule.as_str();
        if let Some(resources) = &self.resources {
            report_shared_resource_metrics(resources);
        }
        for (property, metric) in [
            (
                rocksdb::properties::IS_WRITE_STOPPED,
                "irys.rocksdb.write_stopped",
            ),
            (
                rocksdb::properties::ACTUAL_DELAYED_WRITE_RATE,
                "irys.rocksdb.delayed_write_rate",
            ),
            (
                rocksdb::properties::NUM_RUNNING_COMPACTIONS,
                "irys.rocksdb.running_compactions",
            ),
            (
                rocksdb::properties::NUM_RUNNING_FLUSHES,
                "irys.rocksdb.running_flushes",
            ),
            (
                rocksdb::properties::COMPACTION_PENDING,
                "irys.rocksdb.compaction_pending",
            ),
            (
                rocksdb::properties::BLOCK_CACHE_USAGE,
                "irys.rocksdb.block_cache_usage_bytes",
            ),
            (
                rocksdb::properties::BLOCK_CACHE_CAPACITY,
                "irys.rocksdb.block_cache_capacity_bytes",
            ),
            (
                rocksdb::properties::BLOCK_CACHE_PINNED_USAGE,
                "irys.rocksdb.block_cache_pinned_bytes",
            ),
            (
                rocksdb::properties::CUR_SIZE_ALL_MEM_TABLES,
                "irys.rocksdb.memtable_bytes",
            ),
        ] {
            set_db_gauge(&self.db, property, metric, submodule);
        }
        for cf in families() {
            let Ok(handle) = cf_handle(&self.db, cf) else {
                continue;
            };
            let name = if matches!(cf, Cf::OffsetMeta) {
                self.offset_family_name()
            } else {
                cf.name()
            };
            for (property, metric) in [
                (
                    rocksdb::properties::ESTIMATE_NUM_KEYS,
                    "irys.rocksdb.estimate_num_keys",
                ),
                (
                    rocksdb::properties::LIVE_SST_FILES_SIZE,
                    "irys.rocksdb.live_sst_files_size_bytes",
                ),
                (
                    rocksdb::properties::SIZE_ALL_MEM_TABLES,
                    "irys.rocksdb.memtable_size_bytes",
                ),
                (
                    rocksdb::properties::ESTIMATE_PENDING_COMPACTION_BYTES,
                    "irys.rocksdb.pending_compaction_bytes",
                ),
            ] {
                set_cf_gauge(&self.db, handle, property, metric, submodule, name);
            }
            // Level property is owned. It does not share the `&PropName` table.
            set_cf_gauge(
                &self.db,
                handle,
                rocksdb::properties::num_files_at_level(0),
                "irys.rocksdb.files_at_level0",
                submodule,
                name,
            );
        }
        // Publish even when statistics are off. `absolute` so a 60 s scrape
        // does not double-count. The schema write at open is not included:
        // it runs before this store exists.
        let (compactions, files_before, files_after) = self.compact.state.snapshot();
        for (metric, value) in [
            ("irys.rocksdb.wal_syncs", self.group.wal_syncs()),
            (
                "irys.rocksdb.wal_backstop_syncs",
                self.group.backstop_syncs(),
            ),
            ("irys.rocksdb.small_file_compactions", compactions),
            ("irys.rocksdb.small_file_compact_files_before", files_before),
            ("irys.rocksdb.small_file_compact_files_after", files_after),
        ] {
            metrics::counter!(metric, "submodule" => submodule.to_owned()).absolute(value);
        }
        let Some(stats) = &self.stats else {
            return;
        };
        for (ticker, metric) in [
            (BlockCacheFilterMiss, "irys.rocksdb.block_cache_filter_miss"),
            (BlockCacheFilterHit, "irys.rocksdb.block_cache_filter_hit"),
            (BlockCacheFilterAdd, "irys.rocksdb.block_cache_filter_add"),
            (
                BlockCacheFilterBytesInsert,
                "irys.rocksdb.block_cache_filter_bytes_insert",
            ),
            (BlockCacheIndexMiss, "irys.rocksdb.block_cache_index_miss"),
            (BlockCacheIndexHit, "irys.rocksdb.block_cache_index_hit"),
            (BlockCacheDataMiss, "irys.rocksdb.block_cache_data_miss"),
            (BlockCacheDataHit, "irys.rocksdb.block_cache_data_hit"),
            (BytesRead, "irys.rocksdb.bytes_read"),
            (NumberKeysRead, "irys.rocksdb.keys_read"),
            (StallMicros, "irys.rocksdb.stall_micros"),
            (NoFileOpens, "irys.rocksdb.no_file_opens"),
            (CompactReadBytes, "irys.rocksdb.compact_read_bytes"),
            (CompactWriteBytes, "irys.rocksdb.compact_write_bytes"),
            (FlushWriteBytes, "irys.rocksdb.flush_write_bytes"),
            (WalFileSynced, "irys.rocksdb.wal_file_synced"),
            (WalFileBytes, "irys.rocksdb.wal_file_bytes"),
        ] {
            set_ticker(stats, ticker, metric, submodule);
        }
    }

    /// Flush memtables, compact every column family, and sync the WAL.
    ///
    /// Takes the write lock. Do not call it from inside `update`.
    pub fn flush_and_compact(&self) -> eyre::Result<()> {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for cf in families() {
            let handle = cf_handle(&self.db, cf)?;
            self.db.flush_cf(&handle).wrap_err("flush column family")?;
            self.db
                .compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        }
        // The injected failure stands in for `flush_wal` returning an error.
        // It is taken only for a normal open. A bulk-load copy has no WAL replay.
        #[cfg(any(test, feature = "test-utils"))]
        if !self.bulk_load && self.group.take_fail_next_synced_write() {
            return Err(note_wal_sync_failure(
                &self.group,
                &self.submodule,
                eyre::eyre!(
                    "injected submodule index synced write failure for {}",
                    self.submodule
                ),
            ));
        }
        if let Err(err) = self.db.flush_wal(true) {
            let report = eyre::eyre!("sync wal: {err}");
            // A bulk-load copy disables the WAL. There is nothing to replay.
            if self.bulk_load {
                return Err(report);
            }
            return Err(note_wal_sync_failure(&self.group, &self.submodule, report));
        }
        // The synced WAL covers any open registration group. This flush is
        // the caller's durable point, not a backstop sync.
        self.group.note_wal_sync();
        self.group.cover_durable();
        Ok(())
    }

    /// Cumulative write-stall time, flush jobs, and the compaction gauges.
    ///
    /// Stall time comes from the cumulative line of `rocksdb.dbstats`. That
    /// string property also resets RocksDB's interval counters. Pending
    /// compaction bytes are summed across column families. Flush jobs are
    /// the high-priority compaction count in `cfstats`: RocksDB schedules a
    /// flush on that pool and records the job there. Compactions are low,
    /// bottom, or user priority, so they are not included.
    /// `num-immutable-mem-table-flushed` is only the memtable history, and
    /// that history stays empty with the default maintain size. Running
    /// flushes is the database-wide gauge.
    pub fn ingest_stats(&self) -> eyre::Result<RocksIngestStats> {
        let stats = self
            .db
            .property_value("rocksdb.dbstats")
            .map_err(|error| eyre::eyre!("rocksdb.dbstats: {error}"))?
            .ok_or_else(|| eyre::eyre!("rocksdb.dbstats returned no value"))?;
        let write_stall_micros = parse_cumulative_stall_micros(&stats)
            .ok_or_else(|| eyre::eyre!("rocksdb.dbstats has no cumulative stall line"))?;
        let mut pending_compaction_bytes = 0_u64;
        let mut flushes = 0_u64;
        for cf in families() {
            let handle = cf_handle(&self.db, cf)?;
            pending_compaction_bytes = pending_compaction_bytes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.estimate-pending-compaction-bytes",
            )?);
            flushes = flushes.saturating_add(flush_jobs_cf(&self.db, handle)?);
        }
        if let Some(handle) = self.db.cf_handle("default") {
            pending_compaction_bytes = pending_compaction_bytes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.estimate-pending-compaction-bytes",
            )?);
            flushes = flushes.saturating_add(flush_jobs_cf(&self.db, handle)?);
        }
        let running_flushes = self
            .db
            .property_int_value("rocksdb.num-running-flushes")
            .map_err(|error| eyre::eyre!("rocksdb.num-running-flushes: {error}"))?
            .unwrap_or(0);
        let (
            small_file_compactions,
            small_file_compact_files_before,
            small_file_compact_files_after,
        ) = self.compact.state.snapshot();
        Ok(RocksIngestStats {
            write_stall_micros,
            pending_compaction_bytes,
            flushes,
            running_flushes,
            wal_syncs: self.group.wal_syncs(),
            wal_backstop_syncs: self.group.backstop_syncs(),
            small_file_compactions,
            small_file_compact_files_before,
            small_file_compact_files_after,
        })
    }

    #[cfg(test)]
    pub(super) fn interval_row_count(&self) -> eyre::Result<usize> {
        let handle = cf_handle(&self.db, Cf::Interval)?;
        let snap = self.db.snapshot();
        let mut count = 0;
        for item in snap.iterator_cf(&handle, IteratorMode::Start) {
            item.wrap_err("scan tx intervals")?;
            count += 1;
        }
        Ok(count)
    }

    /// Latest committed WAL sequence. A read leaves it unchanged.
    pub(super) fn committed_sequence(&self) -> eyre::Result<u64> {
        Ok(self.db.latest_sequence_number())
    }

    /// Delete presence rows in each inclusive span.
    ///
    /// Consecutive pieces share one synced batch while their offset count stays
    /// within [`DELETE_SUBRANGE_OFFSETS`]. The batch holds the content keys for
    /// that group only.
    pub(super) fn delete_presence_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        let mut total = PresenceReconcile::default();
        for group in group_presence_pieces(presence_subspans(self, ranges)?) {
            let part = self.delete_presence_ranges_one(&group)?;
            total.path_hash_rows = total.path_hash_rows.saturating_add(part.path_hash_rows);
            total.data_path_rows = total.data_path_rows.saturating_add(part.data_path_rows);
        }
        Ok(total)
    }

    /// Delete presence rows in each inclusive span.
    ///
    /// Counts keys only, then `DeleteRange` removes them. RocksDB v1 also
    /// reads each data-proof value in a span that has rows and point-deletes
    /// its content-hash key in the same batch. A span with no presence key
    /// does not enter the batch and does not read values. An empty result
    /// does not write, so the sequence stays put.
    fn delete_presence_ranges_one(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        if ranges.iter().all(|(start, end)| start > end) {
            return Ok(PresenceReconcile::default());
        }
        let spans = coalesced_u32_spans(ranges);
        if spans.is_empty() {
            return Ok(PresenceReconcile::default());
        }
        let _guard = self.lock_write();
        let snap = self.db.snapshot();
        let path_hash_rows = 0_u64;
        let mut counts = vec![0_u64; spans.len()];
        let mut sparse_keys = vec![Vec::<Vec<u8>>::new(); spans.len()];
        let mut content_deletes = Vec::new();
        // One iterator for every span. A fresh iterator per span rebuilds the
        // range-tombstone map, which dominates when the family is fragmented.
        scan_offset_spans(&self.db, &snap, Cf::OffsetMeta, &spans, |key, value| {
            let offset = PartitionChunkOffset::decode(key)?;
            let Some(index) = span_of(&spans, offset.0) else {
                return Ok(false);
            };
            counts[index] = counts[index].saturating_add(1);
            if (counts[index] as usize) <= RANGE_TOMBSTONE_KEY_LIMIT {
                sparse_keys[index].push(key.to_vec());
            }
            let meta = stored_offset_meta(value)?;
            if let Some(hash) = meta.proof.as_deref().and_then(content_hash_of) {
                content_deletes.push(records::content_key(&hash, offset).to_vec());
            }
            Ok(false)
        })?;
        drop(snap);
        let data_path_rows = counts
            .iter()
            .fold(0_u64, |sum, count| sum.saturating_add(*count));
        if data_path_rows == 0 {
            return Ok(PresenceReconcile {
                path_hash_rows,
                data_path_rows,
            });
        }
        let mut batch = WriteBatch::new();
        let data = cf_handle(&self.db, Cf::OffsetMeta)?;
        for (index, count) in counts.iter().copied().enumerate() {
            if count == 0 {
                continue;
            }
            if sparse_keys[index].len() as u64 == count {
                for key in &sparse_keys[index] {
                    batch.delete_cf(&data, key);
                }
            } else {
                let (start, end) = spans[index];
                batch.delete_range_cf(
                    &data,
                    encode_key(PartitionChunkOffset(start)),
                    exclusive_end_key(PartitionChunkOffset(end)),
                );
            }
        }
        if !content_deletes.is_empty() {
            let handle = cf_handle(&self.db, Cf::Content)?;
            for key in &content_deletes {
                batch.delete_cf(&handle, key);
            }
        }
        self.write_synced_batch(batch, "reconcile presence rows")?;
        Ok(PresenceReconcile {
            path_hash_rows,
            data_path_rows,
        })
    }

    /// Drop proof rows in each span. Keep a checksum-only row.
    ///
    /// Consecutive pieces share one synced batch under the same offset bound as
    /// [`Self::delete_presence_ranges`].
    fn retain_entropy_crc_rows(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        let mut total = PresenceReconcile::default();
        for group in group_presence_pieces(presence_subspans(self, ranges)?) {
            let part = self.retain_entropy_crc_rows_one(&group)?;
            total.path_hash_rows = total.path_hash_rows.saturating_add(part.path_hash_rows);
            total.data_path_rows = total.data_path_rows.saturating_add(part.data_path_rows);
        }
        Ok(total)
    }

    /// Drop proof rows in each span. Keep a checksum-only row.
    ///
    /// A proof row's checksum was computed over data bytes. After the span
    /// is entropy those bytes are gone, so the row is dropped and the offset
    /// is unknown until the next entropy write.
    fn retain_entropy_crc_rows_one(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        if ranges.iter().all(|(start, end)| start > end) {
            return Ok(PresenceReconcile::default());
        }
        let spans = coalesced_u32_spans(ranges);
        if spans.is_empty() {
            return Ok(PresenceReconcile::default());
        }
        let _guard = self.lock_write();
        let snap = self.db.snapshot();
        let mut deletes = Vec::new();
        let mut content_deletes = Vec::new();
        scan_offset_spans(&self.db, &snap, Cf::OffsetMeta, &spans, |key, value| {
            let meta = stored_offset_meta(value)?;
            if meta.proof.is_some() {
                deletes.push(key.to_vec());
                if let Ok(offset) = PartitionChunkOffset::decode(key)
                    && let Some(hash) = meta.proof.as_deref().and_then(content_hash_of)
                {
                    content_deletes.push(records::content_key(&hash, offset).to_vec());
                }
            }
            Ok(false)
        })?;
        drop(snap);
        let data_path_rows = u64::try_from(deletes.len()).unwrap_or(u64::MAX);
        if deletes.is_empty() {
            return Ok(PresenceReconcile::default());
        }
        let mut batch = WriteBatch::new();
        let data = cf_handle(&self.db, Cf::OffsetMeta)?;
        for key in &deletes {
            batch.delete_cf(&data, key);
        }
        if !content_deletes.is_empty() {
            let content = cf_handle(&self.db, Cf::Content)?;
            for key in &content_deletes {
                batch.delete_cf(&content, key);
            }
        }
        self.write_synced_batch(batch, "drop entropy proof rows")?;
        Ok(PresenceReconcile {
            path_hash_rows: 0,
            data_path_rows,
        })
    }

    #[cfg(test)]
    fn flush_memtables(&self) -> eyre::Result<()> {
        let _guard = self.lock_write();
        for cf in families() {
            let handle = cf_handle(&self.db, cf)?;
            self.db.flush_cf(&handle).wrap_err("flush column family")?;
        }
        Ok(())
    }

    /// Live blob files in one column family. `0` when the version has none.
    #[cfg(test)]
    fn blob_file_count(&self, cf: Cf) -> eyre::Result<u64> {
        let handle = cf_handle(&self.db, cf)?;
        self.db
            .property_int_value_cf(&handle, "rocksdb.num-blob-files")
            .wrap_err("read rocksdb.num-blob-files")?
            .ok_or_else(|| eyre::eyre!("rocksdb.num-blob-files missing for {}", cf.name()))
    }

    /// Live SST files in one column family, summed across levels.
    #[cfg(test)]
    fn sst_file_count(&self, cf: Cf) -> eyre::Result<u64> {
        let handle = cf_handle(&self.db, cf)?;
        Ok(live_sst_count(&self.db, handle))
    }

    /// Flush, then compact one family and rewrite the bottom level.
    ///
    /// A plain `compact_range` can trivial-move an SST and leave its blob
    /// indexes in place. Forcing the bottom level runs the compaction
    /// iterator, which inlines a legacy blob when blob files are off.
    #[cfg(test)]
    fn compact_column_force(&self, cf: Cf) -> eyre::Result<()> {
        let _guard = self.lock_write();
        let handle = cf_handle(&self.db, cf)?;
        self.db.flush_cf(&handle).wrap_err("flush column family")?;
        let mut opts = rocksdb::CompactOptions::default();
        opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
        opts.set_exclusive_manual_compaction(true);
        self.db
            .compact_range_cf_opt(&handle, None::<&[u8]>, None::<&[u8]>, &opts);
        Ok(())
    }

    /// Keep flushed L0 files in place so a test can pile up tiny SSTs.
    #[cfg(test)]
    fn disable_auto_compaction(&self) -> eyre::Result<()> {
        for cf in families() {
            let handle = cf_handle(&self.db, cf)?;
            self.db
                .set_options_cf(handle, &[("disable_auto_compactions", "true")])
                .wrap_err("disable auto compaction")?;
        }
        Ok(())
    }

    /// Both directions of every RocksDB v1 family.
    ///
    /// An interval that is not awaiting info has a data-root row with the
    /// same end, tx start, and data size, and that row names the interval.
    /// A pending value names the data root of the interval that covers its
    /// key. A data-path key lies inside an interval. The content-hash index
    /// is the set of proof leaves, compared by a key-order scan. An interval
    /// that still awaits its info is a finding: a finished write fills it.
    pub fn audit_index(&self) -> eyre::Result<Vec<String>> {
        let snap = self.db.snapshot();
        let mut intervals = std::collections::BTreeMap::<u32, records::PlacementRecord>::new();
        {
            let handle = cf_handle(&self.db, Cf::Interval)?;
            for item in snap.iterator_cf(&handle, IteratorMode::Start) {
                let (key, value) = item.wrap_err("scan placement intervals")?;
                let start = PartitionChunkOffset::decode(&key)?.0;
                intervals.insert(start, records::decode_placement(&value)?);
            }
        }
        let covers = |offset: u32| {
            intervals
                .range(..=offset)
                .next_back()
                .is_some_and(|(start, record)| *start <= offset && record.end >= offset)
        };
        let mut findings = Vec::new();
        let mut points =
            std::collections::BTreeMap::<(H256, u32), records::DataRootPlacement>::new();
        {
            let handle = cf_handle(&self.db, Cf::DataRoots)?;
            for item in snap.iterator_cf(&handle, IteratorMode::Start) {
                let (key, value) = item.wrap_err("scan data-root placements")?;
                let (root, start) = records::decode_data_root_key(&key)?;
                points.insert((root, start), records::decode_data_root(&value)?);
            }
        }
        for (start, record) in &intervals {
            if records::awaits_info(record) {
                findings.push(format!("interval at {start} still awaits a data-root info"));
                continue;
            }
            let root = H256(record.data_root);
            match points.get(&(root, *start)) {
                None => findings.push(format!(
                    "interval at {start} for {root} has no data-root row"
                )),
                Some(point)
                    if point.end != record.end
                        || point.tx_start != record.tx_start
                        || point.data_size != record.data_size =>
                {
                    findings.push(format!(
                        "data-root row {root} at {start} does not match its interval"
                    ));
                }
                Some(_) => {}
            }
        }
        for ((root, start), point) in &points {
            match intervals.get(start) {
                Some(record)
                    if H256(record.data_root) == *root
                        && record.end == point.end
                        && record.tx_start == point.tx_start
                        && record.data_size == point.data_size => {}
                Some(_) => findings.push(format!(
                    "data-root placement {root} at {start} does not match its interval"
                )),
                None => findings.push(format!(
                    "data-root placement {root} at {start} has no interval"
                )),
            }
        }
        {
            let handle = cf_handle(&self.db, Cf::OffsetMeta)?;
            for item in snap.iterator_cf(&handle, IteratorMode::Start) {
                let (key, value) = item.wrap_err("scan data paths")?;
                let offset = PartitionChunkOffset::decode(&key)?.0;
                let meta = stored_offset_meta(&value)?;
                // A checksum-only row is an entropy chunk. It is not a proof
                // and does not have to sit inside a placement.
                if meta.proof.is_some() && !covers(offset) {
                    findings.push(format!("data path {offset} is outside every interval"));
                }
            }
        }
        {
            let handle = cf_handle(&self.db, Cf::Pending)?;
            for item in snap.iterator_cf(&handle, IteratorMode::Start) {
                let (key, value) = item.wrap_err("scan pending body migrations")?;
                let offset = PartitionChunkOffset::decode(&key)?.0;
                let Some((start, record)) = intervals
                    .range(..=offset)
                    .next_back()
                    .filter(|(start, record)| **start <= offset && record.end >= offset)
                else {
                    findings.push(format!("pending job {offset} is outside every interval"));
                    continue;
                };
                let job = records::decode_pending(&value)?;
                if job.data_root != H256(record.data_root) {
                    findings.push(format!(
                        "pending job {offset} data root {} does not match interval {start}",
                        job.data_root
                    ));
                }
                if record.data_size != 0 && job.data_size != record.data_size {
                    findings.push(format!(
                        "pending job {offset} data size {} does not match interval {start}",
                        job.data_size
                    ));
                }
            }
        }
        {
            let handle = cf_handle(&self.db, Cf::Content)?;
            let mut iter = snap.raw_iterator_cf_opt(&handle, full_scan_options());
            iter.seek_to_first();
            while let Some(key) = iter.key() {
                let key_owned = key.to_vec();
                let value = iter.value().unwrap_or(&[]).to_vec();
                if !value.is_empty() {
                    findings.push("content-hash row value is not empty".to_string());
                }
                match records::decode_content_key(&key_owned) {
                    Some((_, offset)) if covers(offset.0) => {}
                    Some((_, offset)) => findings.push(format!(
                        "content-hash row at {} is outside every interval",
                        offset.0
                    )),
                    None => findings.push("content-hash key does not decode".to_string()),
                }
                iter.next();
            }
            iter.status().wrap_err("scan content-hash offset index")?;
        }
        let content = self.audit_content_index()?;
        if content.missing_reverse != 0 {
            findings.push(format!(
                "content-hash index is missing {} reverse row(s)",
                content.missing_reverse
            ));
        }
        if content.dangling_reverse != 0 {
            findings.push(format!(
                "content-hash index has {} dangling reverse row(s)",
                content.dangling_reverse
            ));
        }
        Ok(findings)
    }

    /// Every parseable proof has its reverse row, and every reverse row names
    /// that proof's leaf. A proof shorter than a leaf is not counted.
    ///
    /// Both families are scanned in key order. The sets match when the counts
    /// match and a key-order checksum of `hash‖offset` matches. This does not
    /// point-get the other family.
    pub(super) fn audit_content_index(&self) -> eyre::Result<ContentIndexAudit> {
        let snap = self.db.snapshot();
        let data = cf_handle(&self.db, Cf::OffsetMeta)?;
        let content = cf_handle(&self.db, Cf::Content)?;
        let mut audit = ContentIndexAudit::default();
        let mut from_proofs = (0_u64, 0_u64);
        {
            let mut iter = snap.raw_iterator_cf_opt(&data, full_scan_options());
            iter.seek_to_first();
            while let Some(key) = iter.key() {
                let key_owned = key.to_vec();
                let value = iter.value().unwrap_or(&[]).to_vec();
                let meta = stored_offset_meta(&value)?;
                if let Ok(offset) = PartitionChunkOffset::decode(&key_owned)
                    && let Some(hash) = meta.proof.as_deref().and_then(content_hash_of)
                {
                    audit.proofs_with_leaf = audit.proofs_with_leaf.saturating_add(1);
                    mix_key(&mut from_proofs, &records::content_key(&hash, offset));
                }
                iter.next();
            }
            iter.status().wrap_err("scan data proofs")?;
        }
        let mut from_rows = (0_u64, 0_u64);
        {
            let mut iter = snap.raw_iterator_cf_opt(&content, full_scan_options());
            iter.seek_to_first();
            while let Some(key) = iter.key() {
                audit.reverse_rows = audit.reverse_rows.saturating_add(1);
                mix_key(&mut from_rows, key);
                iter.next();
            }
            iter.status().wrap_err("scan content-hash offset index")?;
        }
        if audit.proofs_with_leaf > audit.reverse_rows {
            audit.missing_reverse = audit.proofs_with_leaf - audit.reverse_rows;
        } else if audit.reverse_rows > audit.proofs_with_leaf {
            audit.dangling_reverse = audit.reverse_rows - audit.proofs_with_leaf;
        } else if from_proofs != from_rows {
            audit.missing_reverse = 1;
            audit.dangling_reverse = 1;
        }
        Ok(audit)
    }

    /// Visible keys. Does not read values. A range tombstone hides its span.
    #[cfg(test)]
    fn cf_key_count(&self, cf: Cf) -> eyre::Result<usize> {
        let handle = cf_handle(&self.db, cf)?;
        let snap = self.db.snapshot();
        let mut iter = snap.raw_iterator_cf_opt(&handle, full_scan_options());
        iter.seek_to_first();
        let mut count = 0_usize;
        while iter.key().is_some() {
            count = count.saturating_add(1);
            iter.next();
        }
        iter.status().wrap_err("count column family keys")?;
        Ok(count)
    }
}

impl super::SubmoduleStore for RocksSubmoduleStore {
    fn drop_presence_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        self.delete_presence_ranges(ranges)
    }

    fn view<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        let snap = self.db.snapshot();
        let mut rows = Rows {
            db: &self.db,
            snap: &snap,
            overlay: None,
            layout: self.layout,
        };
        f(&mut rows)
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        let result = self.apply(true, f)?;
        // A synced write covers earlier unsynced registrations in this WAL.
        // A conversion write has no WAL, so it does not cover a group.
        if !self.bulk_load {
            self.group.cover_durable();
        }
        Ok(result)
    }

    fn drop_uninitialized_coverage(
        &self,
        spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
        chunk_size: u64,
    ) -> eyre::Result<()> {
        super::store::drop_uninitialized_coverage_on(self, spans, chunk_size)
    }

    fn reset_in_progress(&self) -> eyre::Result<bool> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let key = encode_key(MetadataKey::SubmoduleResetInProgress);
        Ok(self
            .db
            .get_cf(&handle, key)
            .wrap_err("read reset marker")?
            .is_some())
    }

    fn set_reset_in_progress(&self) -> eyre::Result<()> {
        self.write_metadata(MetadataKey::SubmoduleResetInProgress, Some(vec![1_u8]))
    }

    fn clear_reset_in_progress(&self) -> eyre::Result<()> {
        self.write_metadata(MetadataKey::SubmoduleResetInProgress, None)
    }

    fn stores_offset_crc(&self) -> bool {
        true
    }

    fn chunk_crc_complete(&self) -> eyre::Result<bool> {
        Ok(self
            .read_raw_metadata(MetadataKey::ChunkCrcComplete)?
            .is_some())
    }

    fn set_chunk_crc_complete(&self, complete: bool) -> eyre::Result<()> {
        // Presence is the flag. Skip the synced write when the stored bit
        // already matches, so a durable point does not fsync a no-op.
        if self
            .read_raw_metadata(MetadataKey::ChunkCrcComplete)?
            .is_some()
            == complete
        {
            return Ok(());
        }
        let value = complete.then(|| vec![1_u8]);
        self.write_metadata(MetadataKey::ChunkCrcComplete, value)
    }

    fn retain_entropy_crc_rows(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        self.retain_entropy_crc_rows(ranges)
    }
}

impl RocksSubmoduleStore {
    pub fn stores_tx_paths(&self) -> bool {
        false
    }

    /// First write stores the value. A different stored value refuses.
    ///
    /// `partition` of `None` leaves the partition key absent, so a later
    /// assignment can bind it. `clear` deletes that key and keeps the
    /// packing record.
    pub fn bind_packing_identity(
        &self,
        partition: Option<H256>,
        packing: &PackingRecord,
    ) -> eyre::Result<()> {
        let encoded = packing.encode();
        match self.read_raw_metadata(MetadataKey::PackingRecord)? {
            Some(existing) if existing.as_slice() == encoded.as_slice() => {}
            Some(existing) => {
                let recorded = match PackingRecord::decode(&existing) {
                    Ok(record) => packing_params_valid_for_module(&record, packing)
                        .unwrap_err()
                        .to_string(),
                    Err(_) => format!(
                        "packing identity does not match this storage module. recorded: {} expected: {packing}",
                        hex_bytes(&existing)
                    ),
                };
                eyre::bail!(
                    "{recorded} at {}. Refusing to open. Reset the storage module before rebinding. `submodule-index-convert` changes the schema and does not rebind this value.",
                    self.path.display()
                )
            }
            None => self.write_raw_metadata(MetadataKey::PackingRecord, &encoded)?,
        }
        if let Some(partition) = partition {
            self.bind_metadata(MetadataKey::PartitionHash, &partition.0, "partition")?;
        }
        Ok(())
    }

    fn bind_metadata(&self, key: MetadataKey, value: &[u8], label: &str) -> eyre::Result<()> {
        match self.read_raw_metadata(key)? {
            Some(existing) if existing.as_slice() == value => Ok(()),
            Some(_) => eyre::bail!(
                "submodule index {label} does not match this storage module at {}. Refusing to open. Reset the storage module before rebinding. `submodule-index-convert` changes the schema and does not rebind this value.",
                self.path.display()
            ),
            None => self.write_raw_metadata(key, value),
        }
    }

    pub(super) fn read_raw_metadata(&self, key: MetadataKey) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        self.db
            .get_cf(&handle, encode_key(key))
            .wrap_err("read submodule metadata")
    }

    fn write_raw_metadata(&self, key: MetadataKey, value: &[u8]) -> eyre::Result<()> {
        self.write_metadata_batch("write submodule metadata", |handle, batch| {
            batch.put_cf(handle, encode_key(key), value);
        })
    }

    /// Synced metadata write. The value is raw. Same WAL barrier as `update`.
    /// Not part of `clear`.
    fn write_metadata(&self, key: MetadataKey, value: Option<Vec<u8>>) -> eyre::Result<()> {
        self.write_metadata_batch("write submodule metadata", |handle, batch| {
            let encoded = encode_key(key);
            match &value {
                Some(bytes) => batch.put_cf(handle, encoded, bytes),
                None => batch.delete_cf(handle, encoded),
            }
        })
    }

    /// Every caller holds the write lock, including [`Self::write_metadata_batch`].
    fn write_synced_batch(&self, batch: WriteBatch, action: &'static str) -> eyre::Result<()> {
        let disable_wal = self.bulk_load;
        // Taken only when this batch syncs the WAL. A bulk-load copy does not.
        #[cfg(any(test, feature = "test-utils"))]
        if !disable_wal && self.group.take_fail_next_synced_write() {
            return Err(note_wal_sync_failure(
                &self.group,
                &self.submodule,
                eyre::eyre!(
                    "injected submodule index synced write failure for {}",
                    self.submodule
                ),
            ));
        }
        if let Err(err) = self.db.write_opt(batch, &write_opts(true, disable_wal)) {
            return Err(latch_wal_write_error(
                &self.group,
                disable_wal,
                &self.submodule,
                eyre::eyre!("{action}: {err}"),
            ));
        }
        if !disable_wal {
            self.group.note_wal_sync();
            self.group.cover_durable();
        }
        Ok(())
    }

    fn write_metadata_batch(
        &self,
        action: &'static str,
        fill: impl FnOnce(&rocksdb::ColumnFamily, &mut WriteBatch),
    ) -> eyre::Result<()> {
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let mut batch = WriteBatch::new();
        fill(handle, &mut batch);
        self.write_synced_batch(batch, action)
    }
}

fn sync_wal_locked(db: &DB, group: &GroupCommit, submodule: &str) -> eyre::Result<()> {
    let Some(generation) = group.dirty_generation() else {
        return Ok(());
    };
    // `FlushWAL(true)` is `SyncWAL`. On an IO error, `WALIOStatusCheck` sets a
    // background error when paranoid checks are on (this open keeps that
    // default). A file-scope WAL error is a hard error, so later writes fail
    // while it stands. RocksDB may clear it (`max_bgerror_resume_count`
    // defaults to `INT_MAX`). The flag below does not clear.
    #[cfg(any(test, feature = "test-utils"))]
    if group.take_fail_next_sync() {
        return Err(note_wal_sync_failure(
            group,
            submodule,
            eyre::eyre!("injected submodule index wal sync failure for {submodule}"),
        ));
    }
    if let Err(err) = db.flush_wal(true) {
        return Err(note_wal_sync_failure(
            group,
            submodule,
            eyre::eyre!("sync submodule index wal for {submodule}: {err}"),
        ));
    }
    group.note_backstop_sync();
    group.mark_synced(generation, true);
    Ok(())
}

fn note_wal_sync_failure(group: &GroupCommit, submodule: &str, err: eyre::Report) -> eyre::Report {
    group.note_wal_failure();
    tracing::error!(submodule, %err, "submodule index wal sync failed");
    err
}

/// A failed WAL write can replay after RocksDB clears its own background error.
/// Stick the process flag unless this batch never wrote a WAL.
fn latch_wal_write_error(
    group: &GroupCommit,
    disable_wal: bool,
    submodule: &str,
    err: eyre::Report,
) -> eyre::Report {
    if disable_wal {
        err
    } else {
        note_wal_sync_failure(group, submodule, err)
    }
}

fn submodule_label(rocks_path: &Path) -> String {
    rocks_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn describe_rocks_metrics() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (name, help) in [
            ("irys.rocksdb.estimate_num_keys", "Estimated live keys in one submodule column family"),
            ("irys.rocksdb.live_sst_files_size_bytes", "Live SST bytes in one submodule column family"),
            ("irys.rocksdb.memtable_size_bytes", "Memtable bytes in one submodule column family"),
            ("irys.rocksdb.pending_compaction_bytes", "Estimated bytes waiting for compaction in one submodule column family"),
            ("irys.rocksdb.files_at_level0", "Level-0 files in one submodule column family"),
            ("irys.rocksdb.write_stopped", "1 when RocksDB has stopped writes on this submodule database"),
            ("irys.rocksdb.delayed_write_rate", "Current delayed write rate for this submodule database, bytes per second"),
            ("irys.rocksdb.running_compactions", "Compactions running on this submodule database"),
            ("irys.rocksdb.running_flushes", "Flushes running on this submodule database"),
            ("irys.rocksdb.compaction_pending", "1 when a compaction is pending on this submodule database"),
            ("irys.rocksdb.block_cache_usage_bytes", "Bytes in the block cache this submodule database uses. A shared cache repeats this total on every submodule. Do not add those series."),
            ("irys.rocksdb.block_cache_capacity_bytes", "Block cache capacity for this submodule database. A shared cache repeats this capacity on every submodule."),
            ("irys.rocksdb.block_cache_pinned_bytes", "Pinned bytes in the block cache this submodule database uses. A shared cache repeats this total on every submodule."),
            ("irys.rocksdb.memtable_bytes", "Memtable bytes in this submodule database, summed across column families"),
            ("irys.rocksdb.shared_block_cache_usage_bytes", "Bytes in the process-wide RocksDB block cache, including the write-buffer charge"),
            ("irys.rocksdb.shared_block_cache_pinned_bytes", "Pinned bytes in the process-wide RocksDB block cache"),
            ("irys.rocksdb.shared_block_cache_capacity_bytes", "Capacity of the process-wide RocksDB block cache"),
            ("irys.rocksdb.shared_write_buffer_usage_bytes", "Memtable bytes charged to the process-wide write buffer manager"),
            ("irys.rocksdb.shared_write_buffer_capacity_bytes", "Budget of the process-wide write buffer manager"),
        ] {
            metrics::describe_gauge!(name, help);
        }
        for (name, help) in [
            ("irys.rocksdb.block_cache_filter_miss", "Block-cache filter misses since this submodule database opened"),
            ("irys.rocksdb.block_cache_filter_hit", "Block-cache filter hits since this submodule database opened"),
            ("irys.rocksdb.block_cache_filter_add", "Block-cache filter inserts since this submodule database opened"),
            ("irys.rocksdb.block_cache_filter_bytes_insert", "Block-cache filter bytes inserted since this submodule database opened"),
            ("irys.rocksdb.block_cache_index_miss", "Block-cache index misses since this submodule database opened"),
            ("irys.rocksdb.block_cache_index_hit", "Block-cache index hits since this submodule database opened"),
            ("irys.rocksdb.block_cache_data_miss", "Block-cache data misses since this submodule database opened"),
            ("irys.rocksdb.block_cache_data_hit", "Block-cache data hits since this submodule database opened"),
            ("irys.rocksdb.bytes_read", "Bytes read by Gets since this submodule database opened"),
            ("irys.rocksdb.keys_read", "Keys read since this submodule database opened"),
            ("irys.rocksdb.stall_micros", "Microseconds spent in write stalls since this submodule database opened"),
            ("irys.rocksdb.no_file_opens", "Table and blob file opens since this submodule database opened"),
            ("irys.rocksdb.compact_read_bytes", "Bytes read by compaction since this submodule database opened"),
            ("irys.rocksdb.compact_write_bytes", "Bytes written by compaction since this submodule database opened"),
            ("irys.rocksdb.flush_write_bytes", "Bytes written by flush since this submodule database opened"),
            ("irys.rocksdb.wal_file_synced", "WAL syncs since this submodule database opened"),
            ("irys.rocksdb.wal_syncs", "Synced WAL writes on this submodule database, including backstop syncs"),
            ("irys.rocksdb.wal_backstop_syncs", "WAL syncs from the backstop thread, an explicit group sync, or shutdown"),
            ("irys.rocksdb.wal_file_bytes", "WAL bytes written since this submodule database opened"),
            ("irys.rocksdb.small_file_compactions", "Count-triggered compact_range calls on this submodule database"),
            ("irys.rocksdb.small_file_compact_files_before", "Live SST files counted before each count-triggered compact_range"),
            ("irys.rocksdb.small_file_compact_files_after", "Live SST files counted after each count-triggered compact_range"),
        ] {
            metrics::describe_counter!(name, help);
        }
        metrics::describe_histogram!(
            "irys.rocksdb.write_batch_bytes",
            metrics::Unit::Bytes,
            "Serialized WriteBatch size for one submodule index commit"
        );
    });
}

fn report_shared_resource_metrics(resources: &RocksResources) {
    metrics::gauge!("irys.rocksdb.shared_block_cache_usage_bytes")
        .set(resources.block_cache_usage() as f64);
    metrics::gauge!("irys.rocksdb.shared_block_cache_pinned_bytes")
        .set(resources.block_cache_pinned_usage() as f64);
    metrics::gauge!("irys.rocksdb.shared_block_cache_capacity_bytes")
        .set(resources.block_cache_capacity() as f64);
    metrics::gauge!("irys.rocksdb.shared_write_buffer_usage_bytes")
        .set(resources.write_buffer_usage() as f64);
    metrics::gauge!("irys.rocksdb.shared_write_buffer_capacity_bytes")
        .set(resources.write_buffer_capacity() as f64);
}

fn read_db_u64(db: &DB, name: impl rocksdb::CStrLike) -> Option<u64> {
    match db.property_int_value(name) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "rocksdb property read failed");
            None
        }
    }
}

fn read_cf_u64(db: &DB, cf: &rocksdb::ColumnFamily, name: impl rocksdb::CStrLike) -> Option<u64> {
    match db.property_int_value_cf(cf, name) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "rocksdb column property read failed");
            None
        }
    }
}

fn set_db_gauge(db: &DB, name: impl rocksdb::CStrLike, metric: &'static str, submodule: &str) {
    let Some(value) = read_db_u64(db, name) else {
        return;
    };
    metrics::gauge!(metric, "submodule" => submodule.to_owned()).set(value as f64);
}

fn set_cf_gauge(
    db: &DB,
    cf: &rocksdb::ColumnFamily,
    name: impl rocksdb::CStrLike,
    metric: &'static str,
    submodule: &str,
    family: &str,
) {
    let Some(value) = read_cf_u64(db, cf, name) else {
        return;
    };
    metrics::gauge!(metric, "submodule" => submodule.to_owned(), "cf" => family.to_owned())
        .set(value as f64);
}

fn set_ticker(
    opts: &Options,
    ticker: rocksdb::statistics::Ticker,
    metric: &'static str,
    submodule: &str,
) {
    metrics::counter!(metric, "submodule" => submodule.to_owned())
        .absolute(opts.get_ticker_count(ticker));
}

fn shared_resources_for(
    tuning: RocksTuning,
    passed: Option<Arc<RocksResources>>,
) -> eyre::Result<Option<Arc<RocksResources>>> {
    match passed {
        Some(resources) => Ok(Some(resources)),
        None if tuning.share_resources => Ok(Some(process_rocks_resources()?)),
        None => Ok(None),
    }
}

/// `flock` `LOCK`, creating the file when it is missing.
///
/// Does not take `fcntl`. RocksDB locks with `fcntl`, and that lock does not
/// conflict with `flock`. Closing this file releases every `fcntl` lock this
/// process holds on `LOCK`, including RocksDB's, so the caller keeps it open
/// until the database has closed.
pub(super) fn acquire_conversion_lock(path: &Path) -> eyre::Result<Arc<File>> {
    let lock_path = path.join("LOCK");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // An existing LOCK is RocksDB's file. Truncating it is a write we do not need.
        .truncate(false)
        .open(&lock_path)
        .wrap_err("open rocksdb LOCK")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        eyre::bail!(
            "refusing to run because the rocksdb LOCK is held, so the node may be running. Stop the node and retry. {err}"
        );
    }
    Ok(Arc::new(file))
}

/// `true` when `file` is still the `LOCK` inode at `path`.
///
/// A replaced directory has a different `LOCK` file, or none. Flocking the
/// path again would lock that new file.
pub(super) fn conversion_lock_protects(file: &File, path: &Path) -> bool {
    let Ok(held) = file.metadata() else {
        return false;
    };
    let Ok(current) = fs::metadata(path.join("LOCK")) else {
        return false;
    };
    held.dev() == current.dev() && held.ino() == current.ino()
}

/// Exclusive non-blocking lock of `LOCK` when the file exists.
///
/// Does not create `LOCK`. A missing file means no process holds it.
/// RocksDB locks with `fcntl(F_SETLK)`. `flock` does not observe that lock,
/// so this takes both. The returned file keeps them until it is dropped.
pub(super) fn hold_rocks_lock(path: &Path) -> eyre::Result<Option<Arc<File>>> {
    let lock_path = path.join("LOCK");
    if !lock_path.is_file() {
        return Ok(None);
    }
    // `F_WRLCK` requires a writable descriptor. The bytes are not written.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .wrap_err("open rocksdb LOCK")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        eyre::bail!(
            "refusing to run because the rocksdb LOCK is held, so the node may be running. Stop the node and retry. {err}"
        );
    }
    let mut range = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &mut range) } != 0 {
        let err = std::io::Error::last_os_error();
        eyre::bail!(
            "refusing to run because the rocksdb LOCK is held, so the node may be running. Stop the node and retry. {err}"
        );
    }
    Ok(Some(Arc::new(file)))
}

/// `flock` on `LOCK` when the file exists. Does not take `fcntl`.
///
/// RocksDB locks with `fcntl`. That lock does not block `flock`, and
/// `flock` does not block it. Seal uses this so it can hold the file that
/// [`hold_rocks_lock`] checks while the database is still open.
fn hold_rocks_flock(path: &Path) -> eyre::Result<Option<File>> {
    let lock_path = path.join("LOCK");
    if !lock_path.is_file() {
        return Ok(None);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .wrap_err("open rocksdb LOCK")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        eyre::bail!(
            "refusing to run because the rocksdb LOCK is held, so the node may be running. Stop the node and retry. {err}"
        );
    }
    Ok(Some(file))
}

fn open_db_read_only(path: &Path) -> eyre::Result<DB> {
    let mut db_opts = Options::default();
    db_opts.create_if_missing(false);
    db_opts.create_missing_column_families(false);
    db_opts.set_max_open_files(-1);
    let names = DB::list_cf(&Options::default(), path).wrap_err("list column families")?;
    let families = names
        .into_iter()
        .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, Options::default()));
    // `false`: a WAL from the last write must not fail the read.
    DB::open_cf_descriptors_read_only(&db_opts, path, families, false)
        .wrap_err("open rocksdb submodule index read-only")
}

fn open_db(
    path: &Path,
    tuning: &RocksTuning,
    collect_stats: bool,
    preload_tables: bool,
    resources: Option<&RocksResources>,
    create_missing: bool,
    extra_families: &[String],
) -> eyre::Result<(DB, Option<Arc<Options>>)> {
    let cache = open_cache(tuning, resources);
    let mut db_opts = Options::default();
    // Missing families are created only for a new directory. An existing
    // database lists every family and opens that list, so a family this
    // binary does not know is not a reason for RocksDB to refuse.
    db_opts.create_if_missing(create_missing);
    db_opts.create_missing_column_families(create_missing);
    // Per-database cap on the process pools. RocksDB only raises a pool.
    db_opts.set_max_background_jobs(tuning.background_jobs);
    db_opts.set_bytes_per_sync(1024 * 1024);
    db_opts.set_wal_bytes_per_sync(1024 * 1024);
    db_opts.set_recycle_log_file_num(tuning.recycle_log_file_num);
    db_opts.set_use_fsync(tuning.use_fsync);
    db_opts.set_max_total_wal_size(tuning.max_total_wal_size);
    // -1 keeps every SST reader. A finite cap drops them, and the next Get
    // reloads the footer, index, and filter.
    db_opts.set_max_open_files(-1);
    db_opts.set_max_file_opening_threads(16);
    if preload_tables {
        // Bench only. Skip table properties that only choose compaction inputs.
        db_opts.set_skip_stats_update_on_db_open(true);
    }
    if collect_stats {
        // Tickers only. Timers and histograms stay off.
        db_opts.enable_statistics();
        db_opts.set_statistics_level(rocksdb::statistics::StatsLevel::ExceptHistogramOrTimers);
        db_opts.set_stats_dump_period_sec(0);
        db_opts.set_stats_persist_period_sec(0);
    }
    if let Some(resources) = resources {
        apply_shared_limits(&mut db_opts, resources)?;
    }

    let families = user_column_families(&cache, tuning, extra_families);
    let db = DB::open_cf_descriptors(&db_opts, path, families)
        .wrap_err("open rocksdb submodule index")?;
    let stats = collect_stats.then(|| Arc::new(db_opts));
    Ok((db, stats))
}

fn apply_shared_limits(db_opts: &mut Options, resources: &RocksResources) -> eyre::Result<()> {
    db_opts.set_env(&resources.env);
    // DBImpl reads the manager from the database options. One manager
    // covers every column family and can be shared across databases.
    db_opts.set_write_buffer_manager(&resources.write_buffer);
    let rate = resources.limits.rate_limit_bytes_per_sec;
    if rate > 0 {
        let rate =
            i64::try_from(rate).wrap_err("rocksdb_rate_limit_bytes_per_sec does not fit in i64")?;
        // The binding builds a new limiter on each call and does not return
        // it. One limiter per database is what this open can do. Writes only
        // (flush and compaction). N databases on one disk get N times this rate.
        db_opts.set_ratelimiter(rate, RATE_LIMIT_REFILL_PERIOD_US, RATE_LIMIT_FAIRNESS);
    }
    Ok(())
}

fn open_cache(tuning: &RocksTuning, resources: Option<&RocksResources>) -> rocksdb::Cache {
    match resources {
        Some(resources) => resources.cache.clone(),
        None => block_cache(tuning),
    }
}

fn user_column_families(
    cache: &rocksdb::Cache,
    tuning: &RocksTuning,
    extra_families: &[String],
) -> Vec<rocksdb::ColumnFamilyDescriptor> {
    let mut families: Vec<_> = families()
        .map(|cf| {
            rocksdb::ColumnFamilyDescriptor::new(cf.name(), column_options(cache, cf, tuning))
        })
        .collect();
    // The binding opens "default" with a private 32 MiB cache when this list
    // omits it. That cache would scale with the database count. Give it this
    // open's cache and write buffer instead. The family stays empty.
    families.push(rocksdb::ColumnFamilyDescriptor::new(
        rocksdb::DEFAULT_COLUMN_FAMILY_NAME,
        column_options(cache, Cf::Metadata, tuning),
    ));
    // A family written by a newer binary. No reader or writer in this
    // process uses the handle. Auto compaction stays off so this open does
    // not rewrite that family's SSTs with default table options.
    for name in extra_families {
        let mut opts = Options::default();
        opts.set_disable_auto_compactions(true);
        families.push(rocksdb::ColumnFamilyDescriptor::new(name, opts));
    }
    families
}

fn cf_property_u64(db: &DB, handle: &impl rocksdb::AsColumnFamilyRef, name: &str) -> u64 {
    db.property_int_value_cf(handle, name)
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn property_u64(db: &DB, name: impl rocksdb::CStrLike) -> eyre::Result<u64> {
    db.property_int_value(name)
        .wrap_err("read rocksdb property")?
        .ok_or_else(|| eyre::eyre!("rocksdb property missing"))
}

fn block_cache(tuning: &RocksTuning) -> rocksdb::Cache {
    let mut opts = rocksdb::LruCacheOptions::default();
    opts.set_capacity(tuning.block_cache_bytes);
    // `-1` is the RocksDB default: at most 64 shards. `0` is one shard.
    opts.set_num_shard_bits(tuning.cache_shard_bits);
    rocksdb::Cache::new_lru_cache_opts(&opts)
}

fn column_options(cache: &rocksdb::Cache, cf: Cf, tuning: &RocksTuning) -> Options {
    let mut table = BlockBasedOptions::default();
    table.set_block_size(tuning.block_bytes);
    // Full filter, 10 bits. `false` is not a block-based filter.
    // Only miss families pay: a seek of a known key never consults a bloom.
    let bloom = cf.uses_bloom(tuning);
    if bloom {
        table.set_bloom_filter(10.0, false);
    }
    // The seek is the 32-byte prefix. A whole-key bloom stores hash‖offset
    // and cannot answer it, so `whole_key_filtering` is off. A full scan
    // sets `total_order_seek`. A prefix seek sets `prefix_same_as_start`,
    // which is what consults this bloom.
    if matches!(cf, Cf::Content) {
        table.set_whole_key_filtering(false);
    }
    if tuning.partition_filters {
        table.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);
        // Partitioned filters require a filter policy. The two-level index
        // stays either way: it is the index, not the bloom.
        if bloom {
            table.set_partition_filters(true);
        }
        table.set_pin_top_level_index_and_filter(true);
    }
    table.set_cache_index_and_filter_blocks(true);
    table.set_pin_l0_filter_and_index_blocks_in_cache(true);
    table.set_block_cache(cache);

    let compression = if tuning.compress {
        DBCompressionType::Lz4
    } else {
        DBCompressionType::None
    };
    let mut opts = Options::default();
    opts.set_block_based_table_factory(&table);
    if matches!(cf, Cf::Content) {
        opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(32));
        opts.set_memtable_prefix_bloom_ratio(0.1);
        opts.set_memtable_whole_key_filtering(false);
    }
    opts.set_compression_type(compression);
    // Leveled sizing. Universal compaction ignores this flag.
    opts.set_level_compaction_dynamic_level_bytes(true);
    opts.set_compaction_style(if tuning.universal {
        DBCompactionStyle::Universal
    } else {
        DBCompactionStyle::Level
    });
    opts.set_target_file_size_base(tuning.target_file_bytes);
    opts.set_compaction_readahead_size(2 * 1024 * 1024);
    // Reads stay buffered. The bench drops those pages with fadvise.
    opts.set_use_direct_io_for_flush_and_compaction(tuning.direct_io);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_max_write_buffer_number(2);
    if cf.uses_blobs() {
        // One cache for SST blocks and for any blob file this family still
        // has. New inline values do not get a second cache.
        opts.set_blob_cache(cache);
        opts.set_blob_compaction_readahead_size(2 * 1024 * 1024);
        if tuning.blobs {
            opts.set_enable_blob_files(true);
            opts.set_min_blob_size(BLOB_MIN_BYTES);
            opts.set_blob_file_size(256 * 1024 * 1024);
            opts.set_blob_compression_type(compression);
            opts.set_enable_blob_gc(true);
        } else {
            // New values stay in the SST. GC still inlines an old blob file.
            opts.set_enable_blob_files(false);
            opts.set_enable_blob_gc(true);
            opts.set_blob_gc_age_cutoff(LEGACY_BLOB_GC_AGE_CUTOFF);
        }
    }
    #[cfg(test)]
    if OPEN_DISABLE_AUTO_COMPACTION.with(std::cell::Cell::get) {
        // A normal open compacts and inlines a blob SST. Tests that must
        // read the blob file set this first. Manual compaction still runs.
        opts.set_disable_auto_compactions(true);
    }
    opts
}

#[cfg(test)]
thread_local! {
    static OPEN_DISABLE_AUTO_COMPACTION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// A non-empty directory without SCHEMA is not a new database.
///
/// `CONVERTING` is an unfinished conversion. `CURRENT` without that file is
/// a first open that crashed before the marker. A directory that already has
/// the RocksDB v1 marker is complete only when `CURRENT` is a file.
/// `CONVERTING` beside both is the crash after the marker and before the
/// sentinel is removed, and open continues. That marker without CURRENT is
/// an unrestored manifest pointer, not a new database. Any other marker is
/// an unrecognised layout.
fn refuse_incomplete_directory(path: &Path) -> eyre::Result<()> {
    if path.join(SCHEMA_FILE).exists() {
        refuse_schema_without_current(path)?;
        return Ok(());
    }
    if !directory_has_entries(path)? {
        return Ok(());
    }
    let converting = path.join("CONVERTING").is_file();
    let current = path.join("CURRENT").is_file();
    if current && !converting {
        eyre::bail!(
            "submodule index {} has CURRENT but no SCHEMA marker and no CONVERTING sentinel. A first open crashed after RocksDB created its files and before the marker was written. Delete this directory and start the node again. --replace-incomplete does not apply, because that flag only deletes a directory that contains CONVERTING.",
            path.display()
        );
    }
    eyre::bail!(
        "submodule index {} has no SCHEMA marker, so the directory is incomplete. Delete this directory and rerun `submodule-index-convert` with --replace-incomplete. Do not start the node on it.",
        path.display()
    );
}

/// SCHEMA is written after CURRENT. A directory that has the RocksDB v1
/// marker and no CURRENT file is not empty, and `create_if_missing` would
/// replace the manifest with an empty database. A different marker is left
/// for the layout check.
fn refuse_schema_without_current(path: &Path) -> eyre::Result<()> {
    let marker = path.join(SCHEMA_FILE);
    if !marker.is_file() || path.join("CURRENT").is_file() {
        return Ok(());
    }
    let text = fs::read_to_string(&marker).wrap_err("read schema marker")?;
    if text != SUBMODULE_SCHEMA_ROCKS_V1 {
        return Ok(());
    }
    eyre::bail!(
        "submodule index {} has a SCHEMA marker and no CURRENT file. The directory is not a new database. Restore CURRENT. This open does not create an empty index.",
        path.display()
    )
}

fn directory_has_entries(path: &Path) -> eyre::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !path.is_dir() {
        return Ok(true);
    }
    Ok(fs::read_dir(path)
        .wrap_err("read rocksdb directory")?
        .next()
        .is_some())
}

/// Schema file already on disk, or RocksDB v1 for an empty directory.
///
/// Any other marker is refused. This open does not rewrite it.
fn resolve_layout(path: &Path, force: Option<IndexLayout>) -> eyre::Result<IndexLayout> {
    let marker = path.join(SCHEMA_FILE);
    if !marker.exists() {
        return Ok(force.unwrap_or(IndexLayout::RocksV1));
    }
    let text = fs::read_to_string(&marker).wrap_err("read schema marker")?;
    if text != SUBMODULE_SCHEMA_ROCKS_V1 {
        eyre::bail!("{}", unrecognised_rocks_layout(&text));
    }
    if force.is_some_and(|forced| forced != IndexLayout::RocksV1) {
        eyre::bail!("{}", unrecognised_rocks_layout(&text));
    }
    Ok(IndexLayout::RocksV1)
}

/// Writes SCHEMA via a temp file and a rename, after the bytes are synced.
fn write_schema_marker_file(dir: &Path, text: &str) -> eyre::Result<()> {
    let tmp = dir.join("SCHEMA.tmp");
    let final_path = dir.join(SCHEMA_FILE);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .wrap_err("create schema marker")?;
        file.write_all(text.as_bytes())
            .wrap_err("write schema marker")?;
        file.sync_all().wrap_err("sync schema marker")?;
    }
    fs::rename(&tmp, &final_path).wrap_err("rename schema marker")?;
    // Without a durable SCHEMA dirent, reopen refuses CURRENT as incomplete.
    sync_dir(dir)
}

/// Families on an existing database, and the names this binary does not use.
///
/// `CURRENT` means the directory is not new. Every RocksDB v1 family must
/// already be there. Creating a missing one would hide lost rows. Families
/// that are not in this binary are returned so open can pass them to
/// RocksDB. They are not read or written.
fn existing_unknown_families(path: &Path) -> eyre::Result<Vec<String>> {
    if !path.join("CURRENT").is_file() {
        return Ok(Vec::new());
    }
    let names = DB::list_cf(&Options::default(), path).wrap_err("list column families")?;
    let mut missing = Vec::new();
    for cf in families() {
        let name = cf.name();
        if !names.iter().any(|found| found == name) {
            missing.push(name);
        }
    }
    if !missing.is_empty() {
        eyre::bail!(
            "submodule index {} is missing column family {}. This open does not create it empty. Restore the directory or reset the storage module.",
            path.display(),
            missing.join(", ")
        );
    }
    let mut unknown = Vec::new();
    for name in names {
        if name == rocksdb::DEFAULT_COLUMN_FAMILY_NAME {
            continue;
        }
        if families().any(|cf| cf.name() == name) {
            continue;
        }
        unknown.push(name);
    }
    Ok(unknown)
}

/// A prefix extractor on an unknown family is not recoverable here.
///
/// Writable recovery flushes any WAL tail into an L0 SST using the options
/// passed to this open. Default table options have no prefix extractor.
/// `nullptr` and a missing key are the families this process may open.
/// Read-only open does not flush, so it does not call this.
fn refuse_unknown_prefix_extractor(path: &Path, unknown: &[String]) -> eyre::Result<()> {
    if unknown.is_empty() {
        return Ok(());
    }
    let opts = latest_options_text(path)?;
    for name in unknown {
        match cf_options_value(&opts, name, "prefix_extractor").as_deref() {
            None | Some("nullptr") => {}
            Some(extractor) => {
                eyre::bail!(
                    "submodule index {} has column family {name} with prefix extractor {extractor}. A writable open would recover that family's WAL with default table options and drop the extractor. This open does not rewrite it.",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn latest_options_text(path: &Path) -> eyre::Result<String> {
    let mut newest: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(path).wrap_err("read rocksdb directory")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(number) = name
            .to_string_lossy()
            .strip_prefix("OPTIONS-")
            .and_then(|rest| rest.parse::<u64>().ok())
        else {
            continue;
        };
        if newest.as_ref().is_none_or(|(seen, _)| number > *seen) {
            newest = Some((number, entry.path()));
        }
    }
    let (_, file) = newest.ok_or_else(|| {
        eyre::eyre!(
            "submodule index {} has an unknown column family and no OPTIONS file. This open does not recover it with default table options.",
            path.display()
        )
    })?;
    fs::read_to_string(file).wrap_err("read rocksdb OPTIONS")
}

fn cf_options_value(opts: &str, cf: &str, key: &str) -> Option<String> {
    let header = format!("[CFOptions \"{cf}\"]");
    let start = opts.find(&header)? + header.len();
    let rest = &opts[start..];
    let section = match rest.find("\n[") {
        Some(end) => &rest[..end],
        None => rest,
    };
    let prefix = format!("{key}=");
    section
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix).map(ToString::to_string))
}

fn log_unknown_families(path: &Path, unknown: &[String]) {
    if unknown.is_empty() {
        return;
    }
    tracing::info!(
        path = %path.display(),
        families = %unknown.join(","),
        "opening column families this binary does not use; auto compaction is off, and they are not read or written"
    );
}

/// RocksDB v1 defines no feature bits. A later change that old binaries can
/// ignore sets a `compat` or `ro_compat` bit and keeps this marker. A change
/// to an existing key or value layout, width, endianness, or meaning needs a
/// new marker and `submodule-index-convert`.
const FEATURE_FLAGS_LEN: usize = 24;
const ROCKS_V1_COMPAT_BITS: u64 = 0;
const ROCKS_V1_RO_COMPAT_BITS: u64 = 0;
const ROCKS_V1_INCOMPAT_BITS: u64 = 0;

struct FeatureBits {
    compat: u64,
    ro_compat: u64,
    incompat: u64,
}

fn u64_le_at(bytes: &[u8], at: usize) -> u64 {
    let mut buf = [0_u8; 8];
    buf.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(buf)
}

fn parse_feature_flags(bytes: &[u8]) -> eyre::Result<FeatureBits> {
    if bytes.len() != FEATURE_FLAGS_LEN {
        eyre::bail!(
            "feature flags are {FEATURE_FLAGS_LEN} bytes, found {}",
            bytes.len()
        );
    }
    Ok(FeatureBits {
        compat: u64_le_at(bytes, 0),
        ro_compat: u64_le_at(bytes, 8),
        incompat: u64_le_at(bytes, 16),
    })
}

fn feature_bit_list(bits: u64) -> String {
    let mut names = Vec::new();
    let mut rest = bits;
    let mut index = 0_u32;
    while rest != 0 {
        if rest & 1 == 1 {
            names.push(index.to_string());
        }
        rest >>= 1;
        index = index.saturating_add(1);
    }
    format!("{} ({bits:#x})", names.join(", "))
}

fn unknown_feature_bits(stored: u64, known: u64) -> u64 {
    stored & !known
}

fn refuse_unknown_feature_bits(
    path: &Path,
    flags: &FeatureBits,
    writable: bool,
) -> eyre::Result<()> {
    let incompat = unknown_feature_bits(flags.incompat, ROCKS_V1_INCOMPAT_BITS);
    if incompat != 0 {
        eyre::bail!(
            "submodule index {} has incompatible feature bits {}. This binary is too old.",
            path.display(),
            feature_bit_list(incompat)
        );
    }
    let ro_compat = unknown_feature_bits(flags.ro_compat, ROCKS_V1_RO_COMPAT_BITS);
    if writable && ro_compat != 0 {
        eyre::bail!(
            "submodule index {} has read-only compatible feature bits {}. This binary is too old for a writable open.",
            path.display(),
            feature_bit_list(ro_compat)
        );
    }
    // Unknown compat bits stay stored and do not change this open.
    let _compat = unknown_feature_bits(flags.compat, ROCKS_V1_COMPAT_BITS);
    Ok(())
}

/// Refuse flags or a schema row this binary must not rewrite, before `DB::open`.
///
/// No `CURRENT` means a new directory: there is no log to flush. A present
/// row is checked with writable rules (`ro_compat` refuses). An absent row
/// stays absent until the writable open stamps it.
fn refuse_unwritable_recovery(path: &Path) -> eyre::Result<()> {
    if !path.join("CURRENT").is_file() {
        return Ok(());
    }
    let db = open_db_read_only(path)?;
    if let Some(flags) = read_feature_flags(&db)? {
        refuse_unknown_feature_bits(path, &flags, true)?;
    }
    refuse_present_schema_row(&db)
}

fn read_feature_flags(db: &DB) -> eyre::Result<Option<FeatureBits>> {
    let handle = cf_handle(db, Cf::Metadata)?;
    let key = encode_key(MetadataKey::FeatureFlags);
    match db.get_cf(&handle, &key).wrap_err("read feature flags")? {
        Some(bytes) => Ok(Some(parse_feature_flags(&bytes)?)),
        None => Ok(None),
    }
}

fn refuse_present_schema_row(db: &DB) -> eyre::Result<()> {
    let handle = cf_handle(db, Cf::Metadata)?;
    let key = encode_key(MetadataKey::DBSchemaVersion);
    match db.get_cf(&handle, &key).wrap_err("read schema row")? {
        Some(found) if found.as_slice() == SUBMODULE_SCHEMA_ROCKS_V1.as_bytes() => Ok(()),
        Some(found) => eyre::bail!(
            "{}",
            unrecognised_rocks_layout(&String::from_utf8_lossy(&found))
        ),
        None => Ok(()),
    }
}

/// Stamp 24 zero bytes when the row is absent on a writable open.
///
/// A present row is checked and left as stored. Read-only open does not write.
fn ensure_feature_flags(
    db: &DB,
    path: &Path,
    writable: bool,
    disable_wal: bool,
) -> eyre::Result<()> {
    match read_feature_flags(db)? {
        Some(flags) => refuse_unknown_feature_bits(path, &flags, writable),
        None if writable => {
            let handle = cf_handle(db, Cf::Metadata)?;
            let key = encode_key(MetadataKey::FeatureFlags);
            let mut batch = WriteBatch::new();
            batch.put_cf(&handle, key, [0_u8; FEATURE_FLAGS_LEN]);
            db.write_opt(batch, &write_opts(true, disable_wal))
                .wrap_err("write feature flags")
        }
        None => Ok(()),
    }
}

fn ensure_schema_row(db: &DB) -> eyre::Result<()> {
    write_schema_row(db, false)
}

fn write_schema_row(db: &DB, disable_wal: bool) -> eyre::Result<()> {
    let handle = cf_handle(db, Cf::Metadata)?;
    let key = encode_key(MetadataKey::DBSchemaVersion);
    let expected = SUBMODULE_SCHEMA_ROCKS_V1.as_bytes();
    match db.get_cf(&handle, &key).wrap_err("read schema row")? {
        Some(found) if found == expected => Ok(()),
        Some(found) => eyre::bail!(
            "{}",
            unrecognised_rocks_layout(&String::from_utf8_lossy(&found))
        ),
        None => {
            let mut batch = WriteBatch::new();
            batch.put_cf(&handle, key, expected);
            db.write_opt(batch, &write_opts(true, disable_wal))
                .wrap_err("write schema row")
        }
    }
}

/// `sync` is ignored when the WAL is off. A disabled WAL has nothing to sync.
fn write_opts(sync: bool, disable_wal: bool) -> WriteOptions {
    let mut opts = WriteOptions::default();
    opts.disable_wal(disable_wal);
    opts.set_sync(sync && !disable_wal);
    opts
}

fn is_rocks_wal_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit())
}

/// Delete numeric `*.log` files. RocksDB creates one at open even when every
/// write disables the WAL. Recycled logs are the same names.
fn remove_rocks_wal_files(dir: &Path) -> eyre::Result<()> {
    fn remove_in(dir: &Path) -> eyre::Result<()> {
        for entry in fs::read_dir(dir).wrap_err("read rocksdb directory")? {
            let entry = entry.wrap_err("read rocksdb directory entry")?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_rocks_wal_name(&name) {
                fs::remove_file(entry.path()).wrap_err("remove rocksdb wal")?;
            }
        }
        Ok(())
    }
    remove_in(dir)?;
    let archive = dir.join("archive");
    if archive.is_dir() {
        remove_in(&archive)?;
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> eyre::Result<()> {
    fs::File::open(dir)
        .wrap_err("open rocksdb directory")?
        .sync_all()
        .wrap_err("sync rocksdb directory")
}

fn families() -> impl Iterator<Item = Cf> {
    Cf::ALL.into_iter()
}

/// Counters a bench step can subtract. Stall time, flush count, WAL syncs,
/// and small-file compact counts are cumulative. Pending compaction bytes
/// and running flushes are gauges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RocksIngestStats {
    pub write_stall_micros: u64,
    pub pending_compaction_bytes: u64,
    pub flushes: u64,
    pub running_flushes: u64,
    /// Synced WAL writes since open, including backstop syncs.
    pub wal_syncs: u64,
    /// Backstop, explicit group sync, and shutdown WAL syncs since open.
    pub wal_backstop_syncs: u64,
    /// Count-triggered `compact_range` calls since open.
    pub small_file_compactions: u64,
    /// Live SST files counted before each of those calls, summed.
    pub small_file_compact_files_before: u64,
    /// Live SST files counted after each of those calls, summed.
    pub small_file_compact_files_after: u64,
}

fn int_property_cf(db: &DB, handle: &rocksdb::ColumnFamily, name: &str) -> eyre::Result<u64> {
    db.property_int_value_cf(handle, name)
        .map_err(|error| eyre::eyre!("{name}: {error}"))
        .map(|value| value.unwrap_or(0))
}

/// Flush jobs since open for one column family.
///
/// RocksDB 10.4 prints them as the `High` priority `Comp(cnt)` in
/// `cfstats-no-file-histogram`. A missing `High` row means no flush job
/// has finished. A `High` row that is not the compaction table is an error:
/// treating it as zero would hide a format change the same way a missing
/// integer property did.
fn flush_jobs_cf(db: &DB, handle: &impl rocksdb::AsColumnFamilyRef) -> eyre::Result<u64> {
    const NAME: &str = "rocksdb.cfstats-no-file-histogram";
    let stats = db
        .property_value_cf(handle, NAME)
        .map_err(|error| eyre::eyre!("{NAME}: {error}"))?
        .ok_or_else(|| eyre::eyre!("{NAME} returned no value"))?;
    parse_high_priority_flush_count(&stats)
}

fn parse_high_priority_flush_count(stats: &str) -> eyre::Result<u64> {
    // `PrintLevelStats` order, split on whitespace. The size column is
    // `BytesToHumanString` (`"0.00 KB"`), so it is two tokens. `Comp(cnt)`
    // is the next token after the thirteen numeric columns that follow.
    const COMP_COUNT_AFTER_FILES: usize = 15;
    let mut total = 0_u64;
    for line in stats.lines() {
        let mut tokens = line.split_whitespace();
        let Some(name) = tokens.next() else {
            continue;
        };
        if name != "High" {
            continue;
        }
        let Some(files) = tokens.next() else {
            continue;
        };
        if !files.contains('/') {
            continue;
        }
        let Some(count) = tokens.nth(COMP_COUNT_AFTER_FILES) else {
            eyre::bail!("rocksdb cfstats high-priority line has no flush count: {line}");
        };
        let count = count.parse::<u64>().map_err(|_| {
            eyre::eyre!("rocksdb cfstats high-priority flush count is not an integer: {line}")
        })?;
        total = total.saturating_add(count);
    }
    Ok(total)
}

/// `rocksdb.dbstats` prints cumulative stall time as `HH:MM:SS.mmm H:M:S`.
fn parse_cumulative_stall_micros(stats: &str) -> Option<u64> {
    let line = stats
        .lines()
        .find(|line| line.starts_with("Cumulative stall:"))?;
    let stamp = line
        .trim_start_matches("Cumulative stall:")
        .split_whitespace()
        .next()?;
    parse_hms_micros(stamp)
}

fn parse_hms_micros(stamp: &str) -> Option<u64> {
    let (hours, rest) = stamp.split_once(':')?;
    let (mins, secs) = rest.split_once(':')?;
    let (whole, frac) = secs.split_once('.')?;
    if frac.len() != 3 {
        return None;
    }
    let hours: u64 = hours.parse().ok()?;
    let mins: u64 = mins.parse().ok()?;
    let whole: u64 = whole.parse().ok()?;
    let millis: u64 = frac.parse().ok()?;
    Some(
        hours.saturating_mul(3_600_000_000)
            + mins.saturating_mul(60_000_000)
            + whole.saturating_mul(1_000_000)
            + millis.saturating_mul(1_000),
    )
}

fn cf_handle(db: &DB, cf: Cf) -> eyre::Result<&rocksdb::ColumnFamily> {
    db.cf_handle(cf.name())
        .ok_or_else(|| eyre::eyre!("missing column family {}", cf.name()))
}

fn encode_key<K: Encode>(key: K) -> Vec<u8> {
    key.encode().as_ref().to_vec()
}

/// Exclusive end of an inclusive offset span. `DeleteRange` is half-open.
///
/// The offset past `u32::MAX` is the 4-byte max key plus a trailing zero.
/// The bytewise comparator sorts that longer key after every 4-byte offset.
fn exclusive_end_key(end: PartitionChunkOffset) -> Vec<u8> {
    match end.0.checked_add(1) {
        Some(next) => encode_key(PartitionChunkOffset(next)),
        None => {
            let mut key = encode_key(end);
            key.push(0);
            key
        }
    }
}

/// `true` when `key` is past the inclusive span `[_, end]`.
///
/// A key that is not 4 bytes still belongs to the span when it sorts before
/// the exclusive end. That is not the end of the scan: `DeleteRange` would
/// remove later offset rows, and a read would return a short vector.
fn offset_key_ends_span(key: &[u8], end: PartitionChunkOffset) -> eyre::Result<bool> {
    match PartitionChunkOffset::decode(key) {
        Ok(offset) => Ok(offset > end),
        Err(_) if key >= exclusive_end_key(end).as_slice() => Ok(true),
        Err(err) => Err(eyre::eyre!(
            "ChunkMetaByOffset key of {} bytes is inside the span and is not an offset: {err}",
            key.len()
        )),
    }
}

/// Exclusive end of every key in `cf`.
///
/// Offset families use a 4-byte big-endian key. Data-root and content keys
/// are 36 bytes. The extra `0` sorts after the all-`0xFF` key of that width,
/// so the half-open range covers the last key. `clear` does not range-delete
/// `Metadata`.
fn full_cf_end(cf: Cf) -> Vec<u8> {
    match cf {
        Cf::OffsetMeta | Cf::Pending | Cf::Interval => {
            exclusive_end_key(PartitionChunkOffset(u32::MAX))
        }
        Cf::DataRoots | Cf::Content | Cf::Metadata => {
            // Data-root keys and content-hash keys are 36 bytes. A 32-byte
            // end sorts before them.
            let mut key = vec![0xFF; 36];
            key.push(0);
            key
        }
    }
}

/// Counts from [`RocksSubmoduleStore::audit_content_index`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ContentIndexAudit {
    pub proofs_with_leaf: u64,
    pub reverse_rows: u64,
    pub missing_reverse: u64,
    pub dangling_reverse: u64,
}

/// Scan every key. A prefix extractor would otherwise stop `Next` at the
/// first prefix, and a full clear would skip its `DeleteRange`.
fn full_scan_options() -> ReadOptions {
    let mut opts = ReadOptions::default();
    opts.fill_cache(false);
    opts.set_total_order_seek(true);
    opts
}

/// Order-independent mix of one key. Equal counts plus both accumulators
/// means the two scans saw the same set.
fn mix_key(state: &mut (u64, u64), bytes: &[u8]) {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    state.0 ^= hash;
    state.1 = state.1.wrapping_add(hash);
}

/// Offset value is the versioned record from `encode_offset_meta`.
fn stored_offset_meta(bytes: &[u8]) -> eyre::Result<OffsetMeta> {
    records::decode_offset_meta(bytes)
}

/// Leaf `data_hash` of a data proof. A value shorter than a leaf is not a
/// content-addressed proof and has no reverse row. The hash is the leaf
/// already stored in the proof. Chunk bytes are not hashed again.
fn content_hash_of(proof: &[u8]) -> Option<H256> {
    const LEAF_LEN: usize = 64;
    if proof.len() < LEAF_LEN {
        return None;
    }
    let tail = proof[proof.len() - LEAF_LEN..].to_vec();
    let leaf = irys_types::get_leaf_proof(&irys_types::Base64(tail)).ok()?;
    irys_types::ProofDeserialize::hash(&leaf).map(H256)
}

/// `true` when the family has a key. Does not read the value.
fn cf_has_key(db: &DB, snap: &rocksdb::Snapshot<'_>, cf: Cf) -> eyre::Result<bool> {
    let handle = cf_handle(db, cf)?;
    let mut iter = snap.raw_iterator_cf_opt(&handle, full_scan_options());
    iter.seek_to_first();
    let present = iter.key().is_some();
    iter.status().wrap_err("seek column family")?;
    Ok(present)
}

/// One synced delete covers at most this many offsets: one 36-byte content
/// key per offset. Callers publish the non-`Data` interval image before the
/// delete. Open drops rows that remain under a non-`Data` interval, so a
/// crash between these commits leaves a cleared prefix.
pub const DELETE_SUBRANGE_OFFSETS: u32 = 65_536;

/// Inclusive sub-spans of at most [`DELETE_SUBRANGE_OFFSETS`] offsets.
///
/// No forward span yields nothing. One coalesced span that was split and
/// holds no row also yields nothing: one iterator per piece would not write.
fn presence_subspans(
    store: &RocksSubmoduleStore,
    ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
    let pieces = offset_subspans(ranges);
    if pieces.len() <= 1 || !contiguous_pieces(&pieces) {
        return Ok(pieces);
    }
    let start = pieces[0].0;
    let end = pieces[pieces.len() - 1].1;
    if span_has_offset_row(&store.db, start, end)? {
        Ok(pieces)
    } else {
        Ok(Vec::new())
    }
}

/// Consecutive pieces share one batch while the summed offset count stays
/// within [`DELETE_SUBRANGE_OFFSETS`]. A group is never wider than that, so
/// one commit cannot hold more content keys than the bound allows.
fn group_presence_pieces(
    pieces: Vec<(PartitionChunkOffset, PartitionChunkOffset)>,
) -> Vec<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut offsets = 0_u32;
    for piece in pieces {
        let width = piece.1.0.saturating_sub(piece.0.0).saturating_add(1);
        if !group.is_empty() && offsets.saturating_add(width) > DELETE_SUBRANGE_OFFSETS {
            groups.push(std::mem::take(&mut group));
            offsets = 0;
        }
        offsets = offsets.saturating_add(width);
        group.push(piece);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

fn contiguous_pieces(pieces: &[(PartitionChunkOffset, PartitionChunkOffset)]) -> bool {
    pieces
        .windows(2)
        .all(|pair| pair[0].1.0.checked_add(1) == Some(pair[1].0.0))
}

/// `true` when `[start, end]` holds any offset-meta key. Does not read the value.
fn span_has_offset_row(
    db: &DB,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<bool> {
    if start > end {
        return Ok(false);
    }
    let handle = cf_handle(db, Cf::OffsetMeta)?;
    let snap = db.snapshot();
    let mut opts = full_scan_options();
    opts.set_iterate_upper_bound(exclusive_end_key(end));
    let mut iter = snap.raw_iterator_cf_opt(&handle, opts);
    iter.seek(encode_key(start));
    let present = iter.key().is_some();
    iter.status().wrap_err("seek offset span")?;
    Ok(present)
}

fn offset_subspans(
    ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
    let mut pieces = Vec::new();
    for (start, end) in coalesced_u32_spans(ranges) {
        let mut cursor = start;
        loop {
            let span_end = if end - cursor >= DELETE_SUBRANGE_OFFSETS {
                cursor + (DELETE_SUBRANGE_OFFSETS - 1)
            } else {
                end
            };
            pieces.push((PartitionChunkOffset(cursor), PartitionChunkOffset(span_end)));
            if span_end == end {
                break;
            }
            cursor = span_end + 1;
        }
    }
    pieces
}

/// Content-hash keys held from one pass of a clear scan.
///
/// The scan then drops that batch into the commit overlay and resumes at the
/// first key strictly after the batch-filling key. A seek to the next 4-byte
/// offset would skip a longer key that sorts between the two encodings, and
/// the following `DeleteRange` would remove it. Checksum rows are not keys.
/// The commit is still one batch: a failed sync does not publish a prefix of
/// the deletes.
const CLEAR_CONTENT_KEY_BATCH: usize = 1024;

#[cfg(test)]
thread_local! {
    static CLEAR_OFFSET_VALUE_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CLEAR_CONTENT_SCAN_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CLEAR_CONTENT_KEY_BATCH_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

fn clear_content_key_batch() -> usize {
    #[cfg(test)]
    if let Some(batch) = CLEAR_CONTENT_KEY_BATCH_OVERRIDE.with(std::cell::Cell::get) {
        return batch.max(1);
    }
    CLEAR_CONTENT_KEY_BATCH
}

/// Highest number of offset values `delete_content_rows_for_paths` held at once.
#[cfg(test)]
fn reset_clear_offset_value_peak() {
    CLEAR_OFFSET_VALUE_PEAK.with(|cell| cell.set(0));
}

#[cfg(test)]
fn clear_offset_value_peak() -> usize {
    CLEAR_OFFSET_VALUE_PEAK.with(std::cell::Cell::get)
}

/// `retained` is how many offset values the clear is holding.
#[cfg_attr(not(test), expect(unused_variables))]
fn note_clear_offset_values(retained: usize) {
    #[cfg(test)]
    CLEAR_OFFSET_VALUE_PEAK.with(|cell| {
        if retained > cell.get() {
            cell.set(retained);
        }
    });
}

fn note_clear_content_pass() {
    #[cfg(test)]
    CLEAR_CONTENT_SCAN_PASSES.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
fn reset_clear_content_scan_passes() {
    CLEAR_CONTENT_SCAN_PASSES.with(|cell| cell.set(0));
}

#[cfg(test)]
fn clear_content_scan_passes() -> usize {
    CLEAR_CONTENT_SCAN_PASSES.with(std::cell::Cell::get)
}

/// Restores the production batch size when dropped.
#[cfg(test)]
struct ClearContentKeyBatchGuard;

#[cfg(test)]
impl Drop for ClearContentKeyBatchGuard {
    fn drop(&mut self) {
        CLEAR_CONTENT_KEY_BATCH_OVERRIDE.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
fn set_clear_content_key_batch(batch: usize) -> ClearContentKeyBatchGuard {
    CLEAR_CONTENT_KEY_BATCH_OVERRIDE.with(|cell| cell.set(Some(batch.max(1))));
    ClearContentKeyBatchGuard
}

/// Point-delete a span that holds at most this many keys.
///
/// A `DeleteRange` is a range tombstone. The next iterator rebuilds the
/// fragment map for every tombstone in the family. Many single-key spans
/// therefore stay point deletes. A dense span is still one `DeleteRange`.
const RANGE_TOMBSTONE_KEY_LIMIT: usize = 64;

struct SpanKeys {
    count: usize,
    keys: Vec<Vec<u8>>,
}

fn note_span_key(hit: &mut SpanKeys, key: &[u8]) {
    hit.count = hit.count.saturating_add(1);
    if hit.keys.len() < RANGE_TOMBSTONE_KEY_LIMIT {
        hit.keys.push(key.to_vec());
    }
}

/// `true` when `key` is an offset inside one of `ranges`.
///
/// `ranges` is sorted and disjoint (see [`insert_deleted_range`]). A key that
/// is not a 4-byte offset is not hidden. Content and data-root keys are 36 bytes.
fn offset_key_in_ranges(ranges: &[(u32, u32)], key: &[u8]) -> bool {
    let Ok(offset) = PartitionChunkOffset::decode(key) else {
        return false;
    };
    span_of(ranges, offset.0).is_some()
}

/// Index of the coalesced span that contains `offset`.
fn span_of(spans: &[(u32, u32)], offset: u32) -> Option<usize> {
    let idx = spans.partition_point(|(start, _)| *start <= offset);
    idx.checked_sub(1).filter(|&index| spans[index].1 >= offset)
}

/// Insert `[start, end]` and merge anything it touches.
///
/// The vec stays sorted and disjoint, so [`offset_key_in_ranges`] can binary search.
fn insert_deleted_range(ranges: &mut Vec<(u32, u32)>, start: u32, end: u32) {
    if start > end {
        return;
    }
    let mut start = start;
    let mut end = end;
    let index = ranges.partition_point(|(_, span_end)| match span_end.checked_add(1) {
        Some(next) => next < start,
        None => false,
    });
    while index < ranges.len() {
        let (span_start, span_end) = ranges[index];
        let starts_after = match end.checked_add(1) {
            Some(next) => span_start > next,
            None => false,
        };
        if starts_after {
            break;
        }
        if span_start < start {
            start = span_start;
        }
        if span_end > end {
            end = span_end;
        }
        ranges.remove(index);
    }
    ranges.insert(index, (start, end));
}

fn coalesced_u32_spans(ranges: &[(PartitionChunkOffset, PartitionChunkOffset)]) -> Vec<(u32, u32)> {
    coalesce_inclusive_spans(ranges)
        .into_iter()
        .map(|(start, end)| (start.0, end.0))
        .collect()
}

/// Visit keys that fall in `spans`. `visit` returns `Ok(true)` to stop.
///
/// One iterator. A key in a gap seeks to the next span instead of walking
/// that gap. The upper bound is the exclusive end of the last span.
fn scan_offset_spans(
    db: &DB,
    snap: &rocksdb::Snapshot<'_>,
    cf: Cf,
    spans: &[(u32, u32)],
    mut visit: impl FnMut(&[u8], &[u8]) -> eyre::Result<bool>,
) -> eyre::Result<()> {
    if spans.is_empty() {
        return Ok(());
    }
    let handle = cf_handle(db, cf)?;
    let mut opts = full_scan_options();
    opts.set_iterate_upper_bound(exclusive_end_key(PartitionChunkOffset(
        spans[spans.len() - 1].1,
    )));
    let mut iter = snap.raw_iterator_cf_opt(&handle, opts);
    let mut span_index = 0_usize;
    iter.seek(encode_key(PartitionChunkOffset(spans[0].0)));
    while let Some(key) = iter.key() {
        let key_owned = key.to_vec();
        let value_owned = iter.value().unwrap_or(&[]).to_vec();
        let Ok(offset) = PartitionChunkOffset::decode(&key_owned) else {
            iter.next();
            continue;
        };
        while span_index < spans.len() && offset.0 > spans[span_index].1 {
            span_index += 1;
        }
        if span_index >= spans.len() {
            break;
        }
        if offset.0 < spans[span_index].0 {
            iter.seek(encode_key(PartitionChunkOffset(spans[span_index].0)));
            continue;
        }
        if visit(&key_owned, &value_owned)? {
            break;
        }
        iter.next();
    }
    iter.status().wrap_err("scan offset spans")?;
    Ok(())
}

fn row_overlaps_spans(spans: &[(u32, u32)], row_start: u32, row_end: u32) -> bool {
    let idx = spans.partition_point(|(start, _)| *start <= row_end);
    let Some(index) = idx.checked_sub(1) else {
        return false;
    };
    let (span_start, span_end) = spans[index];
    row_start <= span_end && row_end >= span_start
}

fn row_touches_spans(spans: &[(u32, u32)], row_start: u32, row_end: u32) -> bool {
    let idx = spans.partition_point(|(start, _)| *start <= row_start);
    for index in [idx.checked_sub(1), Some(idx)].into_iter().flatten() {
        if index >= spans.len() {
            continue;
        }
        let (span_start, span_end) = spans[index];
        let right = span_end.saturating_add(1);
        if row_start <= right && row_end.saturating_add(1) >= span_start {
            return true;
        }
    }
    false
}

/// Punch every span out of `existing`. `existing` is sorted by start.
///
/// A hole that ends at `u32::MAX` leaves no right remnant. Adjacent rows with
/// the same placement identity merge, including a remnant that touches a
/// neighbor outside the hole.
fn punch_placement_spans(
    existing: &[records::PlacementSpan],
    spans: &[(u32, u32)],
) -> (Vec<u32>, Vec<records::PlacementSpan>) {
    if spans.is_empty() || existing.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut kept = Vec::new();
    let mut span_index = 0_usize;
    for row in existing {
        while span_index < spans.len() && spans[span_index].1 < row.start {
            span_index += 1;
        }
        let mut cursor = row.start;
        let mut local = span_index;
        let mut alive = true;
        while local < spans.len() && spans[local].0 <= row.value.end {
            let (hole_start, hole_end) = spans[local];
            if hole_end < cursor {
                local += 1;
                continue;
            }
            if cursor < hole_start {
                let mut left = *row;
                left.start = cursor;
                left.value.end = hole_start - 1;
                kept.push(left);
            }
            match hole_end.checked_add(1) {
                Some(next) if next <= row.value.end => {
                    cursor = next;
                    local += 1;
                }
                _ => {
                    alive = false;
                    break;
                }
            }
        }
        if alive && cursor <= row.value.end {
            let mut remnant = *row;
            remnant.start = cursor;
            kept.push(remnant);
        }
    }
    kept.sort_by_key(|row| row.start);
    let mut merged: Vec<records::PlacementSpan> = Vec::new();
    for row in kept {
        if let Some(prev) = merged.last_mut()
            && records::same_identity(&prev.value, &row.value)
            && prev.value.end.checked_add(1) == Some(row.start)
        {
            prev.value.end = row.value.end;
            continue;
        }
        merged.push(row);
    }
    let mut old: BTreeMap<u32, records::PlacementSpan> =
        existing.iter().map(|row| (row.start, *row)).collect();
    let mut put = Vec::new();
    for row in &merged {
        match old.remove(&row.start) {
            Some(previous) if previous.value == row.value => {}
            Some(_) | None => put.push(*row),
        }
    }
    let delete: Vec<_> = old.into_keys().collect();
    (delete, put)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[derive(Default)]
struct Overlay {
    maps: [BTreeMap<Vec<u8>, Option<Vec<u8>>>; CF_COUNT],
    cleared: [bool; CF_COUNT],
    /// Inclusive offset spans removed with `DeleteRange` at commit.
    ///
    /// Only offset-keyed families use this. A put stored after the span is
    /// recorded stays in `maps` and is applied after the tombstone.
    deleted_ranges: [Vec<(u32, u32)>; CF_COUNT],
}

struct Rows<'a> {
    db: &'a DB,
    snap: &'a rocksdb::Snapshot<'a>,
    overlay: Option<&'a Overlay>,
    layout: IndexLayout,
}

struct Batch<'a> {
    db: &'a DB,
    snap: &'a rocksdb::Snapshot<'a>,
    overlay: Overlay,
    layout: IndexLayout,
    /// Leaf bindings seen in this batch. RocksDB v1 does not store them.
    bindings: HashMap<H256, TxLeafBinding>,
    /// Interval starts in this batch that still need a data-root info.
    unfilled: Vec<(u32, H256)>,
    origin: Option<records::PlacementOrigin>,
}

impl<'a> Batch<'a> {
    fn rows(&self) -> Rows<'_> {
        Rows {
            db: self.db,
            snap: self.snap,
            overlay: Some(&self.overlay),
            layout: self.layout,
        }
    }

    fn put_bytes(&mut self, cf: Cf, key: &[u8], value: Vec<u8>) {
        self.overlay.maps[cf.index()].insert(key.to_vec(), Some(value));
    }

    /// Replace the offset record and swap the content-hash key from the proof.
    ///
    /// Both fields empty deletes the row. The checksum is not part of the
    /// content hash. A checksum-only row has no content-hash key.
    fn put_offset_record(
        &mut self,
        offset: PartitionChunkOffset,
        meta: OffsetMeta,
    ) -> eyre::Result<()> {
        let key = encode_key(offset);
        let old = self.rows().get_raw(Cf::OffsetMeta, &key)?;
        let old_meta = match &old {
            Some(bytes) => stored_offset_meta(bytes)?,
            None => OffsetMeta::default(),
        };
        let old_hash = old_meta.proof.as_deref().and_then(content_hash_of);
        let new_hash = meta.proof.as_deref().and_then(content_hash_of);
        if old_hash != new_hash
            && let Some(old_hash) = old_hash
        {
            self.delete(Cf::Content, &records::content_key(&old_hash, offset));
        }
        if let Some(new_hash) = new_hash {
            self.put_bytes(
                Cf::Content,
                &records::content_key(&new_hash, offset),
                Vec::new(),
            );
        }
        if meta.crc.is_none() && meta.proof.is_none() {
            self.delete(Cf::OffsetMeta, &key);
            return Ok(());
        }
        let bytes = records::encode_offset_meta(&meta)?;
        self.put_bytes(Cf::OffsetMeta, &key, bytes);
        Ok(())
    }

    fn delete(&mut self, cf: Cf, key: &[u8]) {
        self.overlay.maps[cf.index()].insert(key.to_vec(), None);
    }

    fn assign_placement(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        tx_path_hash: Option<H256>,
    ) -> eyre::Result<()> {
        if start > end {
            return Ok(());
        }
        let existing = self.rows().placement_intervals_touching(start, end)?;
        let next = match tx_path_hash {
            Some(hash) => {
                let data_root = self.bindings.get(&hash).map(|binding| binding.data_root).ok_or_else(|| {
                    eyre::eyre!(
                        "RocksDB v1 interval requires a tx leaf binding in this batch before the interval is written"
                    )
                })?;
                Some(records::fresh_placement(end.0, data_root, self.origin))
            }
            None => None,
        };
        let (delete, put) = records::plan_placement(&existing, start.0, end.0, next);
        self.apply_placement_edit(&existing, &delete, &put)
    }

    fn apply_placement_edit(
        &mut self,
        existing: &[records::PlacementSpan],
        delete: &[u32],
        put: &[records::PlacementSpan],
    ) -> eyre::Result<()> {
        let by_start: HashMap<u32, &records::PlacementSpan> =
            existing.iter().map(|row| (row.start, row)).collect();
        let touched: HashSet<u32> = delete
            .iter()
            .copied()
            .chain(put.iter().map(|row| row.start))
            .collect();
        self.unfilled.retain(|(start, _)| !touched.contains(start));
        for start_at in delete {
            if let Some(old) = by_start.get(start_at) {
                self.delete_data_root_point(&H256(old.value.data_root), *start_at);
            }
            self.delete(Cf::Interval, &encode_key(PartitionChunkOffset(*start_at)));
        }
        for row in put {
            if let Some(old) = by_start.get(&row.start)
                && old.value.data_root != row.value.data_root
            {
                self.delete_data_root_point(&H256(old.value.data_root), row.start);
            }
            self.put_bytes(
                Cf::Interval,
                &encode_key(PartitionChunkOffset(row.start)),
                records::encode_placement(&row.value).to_vec(),
            );
            if records::awaits_info(&row.value) {
                self.unfilled.push((row.start, H256(row.value.data_root)));
            } else {
                self.put_data_root_point(row);
            }
        }
        Ok(())
    }

    /// Clear every span in one read of the intervals that touch them.
    fn clear_placement_spans(
        &mut self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<()> {
        let spans = coalesced_u32_spans(ranges);
        if spans.is_empty() {
            return Ok(());
        }
        let existing = self.rows().placements_touching_spans(&spans)?;
        let (delete, put) = punch_placement_spans(&existing, &spans);
        self.apply_placement_edit(&existing, &delete, &put)
    }

    fn delete_data_root_point(&mut self, data_root: &H256, start: u32) {
        self.delete(Cf::DataRoots, &records::data_root_key(data_root, start));
    }

    fn put_data_root_point(&mut self, row: &records::PlacementSpan) {
        let value = records::DataRootPlacement {
            end: row.value.end,
            tx_start: row.value.tx_start,
            data_size: row.value.data_size,
        };
        self.put_bytes(
            Cf::DataRoots,
            &records::data_root_key(&H256(row.value.data_root), row.start),
            records::encode_data_root(&value).to_vec(),
        );
    }

    fn reject_empty_info(info: &DataRootInfo) -> eyre::Result<()> {
        if info.data_size == 0 {
            eyre::bail!(
                "RocksDB v1 rejects a data_size of 0; that value marks an unfilled placement"
            );
        }
        Ok(())
    }

    /// Write `info` onto the interval at `start` and its one data-root row.
    fn fill_info_at(&mut self, start: u32, info: &DataRootInfo) -> eyre::Result<()> {
        Self::reject_empty_info(info)?;
        let key = encode_key(PartitionChunkOffset(start));
        let Some(bytes) = self.rows().get_raw(Cf::Interval, &key)? else {
            eyre::bail!("RocksDB v1 data-root info at {start} has no interval");
        };
        let mut value = records::decode_placement(&bytes)?;
        value.tx_start = info.start_offset.0;
        value.data_size = info.data_size;
        self.put_bytes(
            Cf::Interval,
            &key,
            records::encode_placement(&value).to_vec(),
        );
        self.put_data_root_point(&records::PlacementSpan { start, value });
        self.unfilled.retain(|(unfilled, _)| *unfilled != start);
        Ok(())
    }

    fn fill_info(&mut self, data_root: H256, info: &DataRootInfo) -> eyre::Result<()> {
        Self::reject_empty_info(info)?;
        let starts: Vec<u32> = self
            .unfilled
            .iter()
            .filter(|(_, root)| *root == data_root)
            .map(|(start, _)| *start)
            .collect();
        let start = match starts.as_slice() {
            [start] => *start,
            [] => eyre::bail!(
                "RocksDB v1 has no unfilled placement for data root {data_root} in this batch"
            ),
            _ => eyre::bail!(
                "RocksDB v1 has more than one unfilled placement for data root {data_root}; name the interval start"
            ),
        };
        self.fill_info_at(start, info)
    }

    fn replace_infos(&mut self, data_root: DataRoot, infos: &DataRootInfos) -> eyre::Result<()> {
        // A data-root row's key is the interval start. Deleting it while that
        // interval remains leaves a placement with no secondary row. Adding a
        // row at `max(tx_start, 0)` invents a key no interval owns.
        let existing = self.rows().data_root_rows(&data_root)?;
        for (start, point) in &existing {
            let kept = infos.0.iter().any(|info| {
                info.start_offset.0 == point.tx_start && info.data_size == point.data_size
            });
            if kept {
                continue;
            }
            let still_owned = match self
                .rows()
                .get_raw(Cf::Interval, &encode_key(PartitionChunkOffset(*start)))?
            {
                Some(bytes) => records::decode_placement(&bytes)?.data_root == data_root.0,
                None => false,
            };
            if still_owned {
                eyre::bail!(
                    "RocksDB v1 keeps the data-root row while its interval at {start} exists"
                );
            }
            self.delete_data_root_point(&data_root, *start);
        }
        for info in &infos.0 {
            let present = existing.iter().any(|(_, point)| {
                point.tx_start == info.start_offset.0 && point.data_size == info.data_size
            });
            if !present {
                eyre::bail!("RocksDB v1 will not add a data-root row with no interval");
            }
        }
        Ok(())
    }

    /// Remove inclusive offset keys with one `DeleteRange`.
    ///
    /// The key is a 4-byte big-endian offset, so the byte order matches the
    /// offset order. An empty span writes nothing. This does not read values.
    /// Hash-keyed families are not offset spans: a caller that needs a hash
    /// row reads that value.
    fn delete_offset_range(
        &mut self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        if start > end {
            return Ok(());
        }
        if !self.rows().offset_range_has_key(cf, start, end)? {
            return Ok(());
        }
        self.remember_deleted_offset_range(cf, start, end);
        Ok(())
    }

    /// Record a `DeleteRange` the commit will emit. Does not seek.
    ///
    /// The caller already knows the span holds a key. An empty span must not
    /// call this: a tombstone over no key still costs the next iterator.
    fn remember_deleted_offset_range(
        &mut self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let index = cf.index();
        self.overlay.maps[index].retain(|key, _| !offset_key_in_ranges(&[(start.0, end.0)], key));
        insert_deleted_range(&mut self.overlay.deleted_ranges[index], start.0, end.0);
    }

    /// Drop visible keys in `ranges`. A span with few keys is point deletes.
    ///
    /// One snapshot pass places each key. Touching spans share one tombstone
    /// when the span is dense. The count is the keys visible at this call.
    fn delete_offset_spans(
        &mut self,
        cf: Cf,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<usize> {
        let spans = coalesced_u32_spans(ranges);
        if spans.is_empty() {
            return Ok(0);
        }
        let hits = self.rows().visible_span_keys(cf, &spans)?;
        let mut removed = 0_usize;
        for (span, hit) in spans.iter().zip(hits) {
            if hit.count == 0 {
                continue;
            }
            removed = removed.saturating_add(hit.count);
            if hit.keys.len() == hit.count {
                for key in hit.keys {
                    self.delete(cf, &key);
                }
            } else {
                self.remember_deleted_offset_range(
                    cf,
                    PartitionChunkOffset(span.0),
                    PartitionChunkOffset(span.1),
                );
            }
        }
        Ok(removed)
    }

    /// Point-delete reverse rows for proofs currently visible in `[start, end]`.
    ///
    /// One content hash can name many offsets. This deletes `hash‖offset`
    /// only. It does not delete the prefix. The scan drops each offset value
    /// before the next key. A checksum-only row has no content key and is
    /// left for the following `DeleteRange`. Content deletes stay in this
    /// batch until commit, so a failed sync leaves the offset rows in place.
    fn delete_content_rows_for_paths(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let mut next = Some(encode_key(start));
        while let Some(start_key) = next.take() {
            // The cursor borrows the snapshot. One pass keeps a bounded set of
            // content keys, then this loop deletes them and resumes.
            let (content_keys, resume) = {
                let rows = self.rows();
                let mut cursor = rows.cursor(Cf::OffsetMeta, Some(&start_key))?;
                let mut content_keys = Vec::new();
                let mut resume = None;
                note_clear_content_pass();
                while let Some((key, value)) = cursor.next_kv()? {
                    if offset_key_ends_span(&key, end)? {
                        break;
                    }
                    let offset = PartitionChunkOffset::decode(&key)
                        .expect("a key inside the span is a 4-byte offset");
                    // The value dies with this iteration. The cursor holds one
                    // lookahead key, not the rest of the span.
                    note_clear_offset_values(1);
                    let meta = stored_offset_meta(&value)?;
                    drop(value);
                    if let Some(hash) = meta.proof.as_deref().and_then(content_hash_of) {
                        content_keys.push(records::content_key(&hash, offset));
                    }
                    if content_keys.len() == clear_content_key_batch() {
                        // `encode(n + 1)` is the next 4-byte offset. A longer
                        // key sorts after `encode(n)` and before that offset.
                        // The trailing 0 is the first key strictly after `n`.
                        let mut resume_key = key;
                        resume_key.push(0);
                        resume = Some(resume_key);
                        break;
                    }
                }
                (content_keys, resume)
            };
            for key in &content_keys {
                self.delete(Cf::Content, key);
            }
            next = resume;
        }
        Ok(())
    }

    fn commit(
        &self,
        sync: bool,
        disable_wal: bool,
        submodule: &str,
        group: &GroupCommit,
    ) -> eyre::Result<()> {
        let mut batch = WriteBatch::new();
        for cf in families() {
            let handle = cf_handle(self.db, cf)?;
            // One range tombstone covers the family. A point delete per key
            // would read every value, including blobs, into this batch.
            // Dropping the family is not one batch: a crash can leave it missing.
            if self.overlay.cleared[cf.index()] {
                if cf_has_key(self.db, self.snap, cf)? {
                    // Empty begin sorts before every key. `from` and `to` share one type.
                    batch.delete_range_cf(&handle, Vec::<u8>::new(), full_cf_end(cf));
                }
            } else {
                for &(start, end) in &self.overlay.deleted_ranges[cf.index()] {
                    batch.delete_range_cf(
                        &handle,
                        encode_key(PartitionChunkOffset(start)),
                        exclusive_end_key(PartitionChunkOffset(end)),
                    );
                }
            }
            for (key, value) in &self.overlay.maps[cf.index()] {
                match value {
                    Some(value) => batch.put_cf(&handle, key, value),
                    None => batch.delete_cf(&handle, key),
                }
            }
        }
        describe_rocks_metrics();
        // Serialized batch size, including the batch header. This is the
        // application batch, which ticker stats do not record.
        metrics::histogram!(
            "irys.rocksdb.write_batch_bytes",
            "submodule" => submodule.to_owned()
        )
        .record(batch.size_in_bytes() as f64);
        // `disable_wal` is the conversion copy. Those rows become durable when
        // the memtables are flushed. `sync` false still appends the WAL. A
        // later synced write or `flush_wal` makes that group durable.
        // The injected failure stands in for `write_opt` returning an error
        // on a synced batch. An unsynced append does not take it.
        #[cfg(any(test, feature = "test-utils"))]
        if sync && !disable_wal && group.take_fail_next_synced_write() {
            return Err(note_wal_sync_failure(
                group,
                submodule,
                eyre::eyre!("injected submodule index synced write failure for {submodule}"),
            ));
        }
        if let Err(err) = self.db.write_opt(batch, &write_opts(sync, disable_wal)) {
            return Err(latch_wal_write_error(
                group,
                disable_wal,
                submodule,
                eyre::eyre!("commit submodule index batch: {err}"),
            ));
        }
        Ok(())
    }
}

impl Rows<'_> {
    fn get_raw(&self, cf: Cf, key: &[u8]) -> eyre::Result<Option<Vec<u8>>> {
        if let Some(overlay) = self.overlay {
            if let Some(value) = overlay.maps[cf.index()].get(key) {
                return Ok(value.clone());
            }
            if overlay.cleared[cf.index()]
                || offset_key_in_ranges(&overlay.deleted_ranges[cf.index()], key)
            {
                return Ok(None);
            }
        }
        let handle = cf_handle(self.db, cf)?;
        self.snap
            .get_cf(&handle, key)
            .wrap_err("read submodule index")
    }

    fn cursor(&self, cf: Cf, start: Option<&[u8]>) -> eyre::Result<MergeCursor<'_>> {
        self.cursor_bounded(cf, start, None)
    }

    /// `upper` is an exclusive key bound. The iterator does not surface it.
    fn cursor_bounded(
        &self,
        cf: Cf,
        start: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> eyre::Result<MergeCursor<'_>> {
        let handle = cf_handle(self.db, cf)?;
        // A prefix extractor stops `Next` at the first prefix unless the
        // iterator is total order. Prefix lookups do not use this cursor.
        // The block cache stays on: pending and data-root reads are hot.
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
        if let Some(upper) = upper {
            opts.set_iterate_upper_bound(upper);
        }
        let mode = match start {
            Some(key) => IteratorMode::From(key, Direction::Forward),
            None => IteratorMode::Start,
        };
        let hidden = self
            .overlay
            .map(|overlay| overlay.deleted_ranges[cf.index()].clone())
            .unwrap_or_default();
        let db =
            self.snap
                .iterator_cf_opt(&handle, opts, mode)
                .filter_map(move |item| match item {
                    Ok((key, _)) if offset_key_in_ranges(&hidden, &key) => None,
                    Ok((key, value)) => Some(Ok((key.into_vec(), value.into_vec()))),
                    Err(err) => Some(Err(eyre::eyre!("{err}"))),
                });
        let cleared = self
            .overlay
            .is_some_and(|overlay| overlay.cleared[cf.index()]);
        let overlay = self
            .overlay
            .map(|overlay| overlay_pairs(overlay, cf, start, upper))
            .unwrap_or_default();
        Ok(MergeCursor::new(Box::new(db), overlay, cleared))
    }

    fn floor_raw(&self, cf: Cf, key: &[u8]) -> eyre::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let cleared = self
            .overlay
            .is_some_and(|overlay| overlay.cleared[cf.index()]);
        let handle = cf_handle(self.db, cf)?;
        let mut db_iter = (!cleared).then(|| {
            self.snap
                .iterator_cf(&handle, IteratorMode::From(key, Direction::Reverse))
        });
        let mut db_next = match db_iter.as_mut().and_then(Iterator::next) {
            Some(Ok((found_key, value))) => Some((found_key.into_vec(), value.into_vec())),
            Some(Err(err)) => return Err(eyre::eyre!("{err}")),
            None => None,
        };
        let bound = key.to_vec();
        let mut overlay_iter = self
            .overlay
            .map(|overlay| overlay.maps[cf.index()].range(..=bound).rev());
        let mut overlay_next = overlay_iter
            .as_mut()
            .and_then(Iterator::next)
            .map(|(found_key, value)| (found_key.clone(), value.clone()));

        loop {
            let db_key = db_next.as_ref().map(|(found_key, _)| found_key.clone());
            let overlay_key = overlay_next
                .as_ref()
                .map(|(found_key, _)| found_key.clone());
            let take_overlay = match (db_key.as_deref(), overlay_key.as_deref()) {
                (None, None) => return Ok(None),
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (Some(db_key), Some(overlay_key)) => overlay_key >= db_key,
            };
            if take_overlay {
                let (found_key, value) = overlay_next.take().expect("overlay key is pending");
                overlay_next = overlay_iter
                    .as_mut()
                    .and_then(Iterator::next)
                    .map(|(next_key, next_value)| (next_key.clone(), next_value.clone()));
                if db_key.as_deref() == Some(found_key.as_slice()) {
                    db_next = match db_iter.as_mut().and_then(Iterator::next) {
                        Some(Ok((next_key, next_value))) => {
                            Some((next_key.into_vec(), next_value.into_vec()))
                        }
                        Some(Err(err)) => return Err(eyre::eyre!("{err}")),
                        None => None,
                    };
                }
                if let Some(value) = value {
                    return Ok(Some((found_key, value)));
                }
            } else if db_next.as_ref().is_some_and(|(key, _)| {
                self.overlay.is_some_and(|overlay| {
                    offset_key_in_ranges(&overlay.deleted_ranges[cf.index()], key)
                })
            }) {
                db_next = match db_iter.as_mut().and_then(Iterator::next) {
                    Some(Ok((next_key, next_value))) => {
                        Some((next_key.into_vec(), next_value.into_vec()))
                    }
                    Some(Err(err)) => return Err(eyre::eyre!("{err}")),
                    None => None,
                };
            } else {
                return Ok(db_next);
            }
        }
    }

    /// `true` when a visible offset key lies in `[start, end]`.
    ///
    /// The iterator stops at the first hit. `iterate_upper_bound` is the
    /// exclusive end, and `fill_cache` is off. Values are not read.
    fn offset_range_has_key(
        &self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        if start > end {
            return Ok(false);
        }
        if let Some(overlay) = self.overlay
            && overlay.cleared[cf.index()]
        {
            let start_key = encode_key(start);
            let end_key = encode_key(end);
            return Ok(overlay.maps[cf.index()]
                .range(start_key..=end_key)
                .any(|(_, value)| value.is_some()));
        }
        if let Some(overlay) = self.overlay {
            let start_key = encode_key(start);
            let end_key = encode_key(end);
            if overlay.maps[cf.index()]
                .range(start_key..=end_key)
                .any(|(_, value)| value.is_some())
            {
                return Ok(true);
            }
        }
        let Some(key) = self.first_offset_key(cf, start, end)? else {
            return Ok(false);
        };
        if let Some(overlay) = self.overlay
            && (overlay.maps[cf.index()].contains_key(&key)
                || offset_key_in_ranges(&overlay.deleted_ranges[cf.index()], &key))
        {
            // A delete or a DeleteRange in this batch hides that key. Keep
            // scanning the snapshot for a key the overlay does not hide.
            return self.offset_snapshot_has_uncovered_key(cf, start, end);
        }
        Ok(true)
    }

    fn offset_snapshot_has_uncovered_key(
        &self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        let handle = cf_handle(self.db, cf)?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        opts.set_iterate_upper_bound(exclusive_end_key(end));
        let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
        iter.seek(encode_key(start));
        loop {
            let key = iter.key().map(<[u8]>::to_vec);
            let Some(key) = key else {
                break;
            };
            let hidden = self.overlay.is_some_and(|overlay| {
                overlay.maps[cf.index()].contains_key(&key)
                    || offset_key_in_ranges(&overlay.deleted_ranges[cf.index()], &key)
            });
            if !hidden {
                iter.status().wrap_err("seek offset range")?;
                return Ok(true);
            }
            iter.next();
        }
        iter.status().wrap_err("seek offset range")?;
        Ok(false)
    }

    /// First snapshot key in `[start, end]`, without its value.
    fn first_offset_key(
        &self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(self.db, cf)?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        opts.set_iterate_upper_bound(exclusive_end_key(end));
        let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
        iter.seek(encode_key(start));
        let key = iter.key().map(<[u8]>::to_vec);
        iter.status().wrap_err("seek offset range")?;
        Ok(key)
    }

    /// Visible keys in `[start, end]`. Does not read values.
    fn count_offset_keys(
        &self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<usize> {
        if start > end {
            return Ok(0);
        }
        if let Some(overlay) = self.overlay
            && overlay.cleared[cf.index()]
        {
            let start_key = encode_key(start);
            let end_key = encode_key(end);
            return Ok(overlay.maps[cf.index()]
                .range(start_key..=end_key)
                .filter(|(_, value)| value.is_some())
                .count());
        }
        let handle = cf_handle(self.db, cf)?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        opts.set_iterate_upper_bound(exclusive_end_key(end));
        let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
        iter.seek(encode_key(start));
        let mut count = 0_usize;
        loop {
            let key = iter.key().map(<[u8]>::to_vec);
            let Some(key) = key else {
                break;
            };
            let overlay_hit = self
                .overlay
                .and_then(|overlay| overlay.maps[cf.index()].get(&key));
            let ranged = self.overlay.is_some_and(|overlay| {
                offset_key_in_ranges(&overlay.deleted_ranges[cf.index()], &key)
            });
            if !ranged && overlay_hit.is_none() {
                count = count.saturating_add(1);
            }
            iter.next();
        }
        iter.status().wrap_err("count offset keys")?;
        if let Some(overlay) = self.overlay {
            let start_key = encode_key(start);
            let end_key = encode_key(end);
            count = count.saturating_add(
                overlay.maps[cf.index()]
                    .range(start_key..=end_key)
                    .filter(|(_, value)| value.is_some())
                    .count(),
            );
        }
        Ok(count)
    }

    fn interval_covering(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<IntervalRow>> {
        let Some(row) = self.floor_interval(offset)? else {
            return Ok(None);
        };
        Ok((row.start <= offset && row.end >= offset).then_some(row))
    }

    fn intervals_overlapping(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<IntervalRow>> {
        if start > end {
            return Ok(Vec::new());
        }
        let rows = self.intervals_touching(start, end)?;
        Ok(rows
            .into_iter()
            .filter(|row| row.start <= end && row.end >= start)
            .collect())
    }

    fn floor_interval(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<IntervalRow>> {
        let Some((key, value)) = self.floor_raw(Cf::Interval, &encode_key(offset))? else {
            return Ok(None);
        };
        Ok(Some(self.interval_row(&key, &value)?))
    }

    fn interval_row(&self, key: &[u8], value: &[u8]) -> eyre::Result<IntervalRow> {
        let start = PartitionChunkOffset::decode(key)?;

        let record = records::decode_placement(value)?;
        Ok(IntervalRow {
            start,
            end: PartitionChunkOffset(record.end),
            tx_path_hash: H256(record.data_root),
        })
    }

    fn intervals_touching(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<IntervalRow>> {
        let start_key = encode_key(start);
        let floor = self.floor_raw(Cf::Interval, &start_key)?;
        // An exact hit is the row being edited. The previous key is the left
        // neighbor a same-hash assign has to merge with.
        let from_owned = if floor
            .as_ref()
            .is_some_and(|(key, _)| key.as_slice() == start_key.as_slice())
            && start.0 > 0
        {
            let left_key = encode_key(PartitionChunkOffset(start.0 - 1));
            self.floor_raw(Cf::Interval, &left_key)?
                .map(|(key, _)| key)
                .unwrap_or(start_key)
        } else if let Some((key, _)) = floor {
            key
        } else {
            start_key
        };
        let mut cursor = self.cursor(Cf::Interval, Some(&from_owned))?;
        let left_limit = start.0.saturating_sub(1);
        let right_limit = end.0.saturating_add(1);
        let mut rows = Vec::new();
        while let Some((key, value)) = cursor.next_kv()? {
            let offset = PartitionChunkOffset::decode(&key)?;
            let interval = self.interval_row(&key, &value)?;
            if interval.end.0 < left_limit && offset < start {
                continue;
            }
            if offset.0 > right_limit {
                break;
            }
            rows.push(interval);
            if offset.0 >= right_limit {
                break;
            }
        }
        Ok(rows)
    }

    fn placement_intervals_touching(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<records::PlacementSpan>> {
        let rows = self.intervals_touching(start, end)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(bytes) = self.get_raw(Cf::Interval, &encode_key(row.start))? else {
                continue;
            };
            out.push(records::PlacementSpan {
                start: row.start.0,
                value: records::decode_placement(&bytes)?,
            });
        }
        Ok(out)
    }

    /// Visible keys in each span. At most [`RANGE_TOMBSTONE_KEY_LIMIT`] keys are stored.
    fn visible_span_keys(&self, cf: Cf, spans: &[(u32, u32)]) -> eyre::Result<Vec<SpanKeys>> {
        let mut hits = spans
            .iter()
            .map(|_| SpanKeys {
                count: 0,
                keys: Vec::new(),
            })
            .collect::<Vec<_>>();
        let index = cf.index();
        let cleared = self.overlay.is_some_and(|overlay| overlay.cleared[index]);
        if !cleared {
            scan_offset_spans(self.db, self.snap, cf, spans, |key, _| {
                let hidden = self.overlay.is_some_and(|overlay| {
                    overlay.maps[index].contains_key(key)
                        || offset_key_in_ranges(&overlay.deleted_ranges[index], key)
                });
                if !hidden
                    && let Ok(offset) = PartitionChunkOffset::decode(key)
                    && let Some(span_index) = span_of(spans, offset.0)
                {
                    note_span_key(&mut hits[span_index], key);
                }
                Ok(false)
            })?;
        }
        if let Some(overlay) = self.overlay {
            for (key, value) in &overlay.maps[index] {
                if value.is_none() {
                    continue;
                }
                let Ok(offset) = PartitionChunkOffset::decode(key) else {
                    continue;
                };
                let Some(span_index) = span_of(spans, offset.0) else {
                    continue;
                };
                note_span_key(&mut hits[span_index], key);
            }
        }
        Ok(hits)
    }

    fn pending_hit_in_spans(&self, spans: &[(u32, u32)]) -> eyre::Result<bool> {
        if spans.is_empty() {
            return Ok(false);
        }
        let index = Cf::Pending.index();
        if let Some(overlay) = self.overlay {
            let overlay_hit = overlay.maps[index]
                .iter()
                .any(|(key, value)| value.is_some() && offset_key_in_ranges(spans, key));
            if overlay_hit || overlay.cleared[index] {
                return Ok(overlay_hit);
            }
        }
        let mut hit = false;
        scan_offset_spans(self.db, self.snap, Cf::Pending, spans, |key, _| {
            let hidden = self.overlay.is_some_and(|overlay| {
                overlay.maps[index].contains_key(key)
                    || offset_key_in_ranges(&overlay.deleted_ranges[index], key)
            });
            if !hidden {
                hit = true;
                return Ok(true);
            }
            Ok(false)
        })?;
        Ok(hit)
    }

    fn intervals_overlap_u32_spans(&self, spans: &[(u32, u32)]) -> eyre::Result<bool> {
        if spans.is_empty() {
            return Ok(false);
        }
        let index = Cf::Interval.index();
        let cleared = self.overlay.is_some_and(|overlay| overlay.cleared[index]);
        if let Some(overlay) = self.overlay {
            for (key, value) in &overlay.maps[index] {
                let Some(bytes) = value else {
                    continue;
                };
                let Ok(start) = PartitionChunkOffset::decode(key) else {
                    continue;
                };
                let record = records::decode_placement(bytes)?;
                if row_overlaps_spans(spans, start.0, record.end) {
                    return Ok(true);
                }
            }
            if cleared {
                return Ok(false);
            }
        }
        let handle = cf_handle(self.db, Cf::Interval)?;
        let mut opts = full_scan_options();
        opts.set_iterate_upper_bound(exclusive_end_key(PartitionChunkOffset(
            spans[spans.len() - 1].1,
        )));
        let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
        for &(start, end) in spans {
            iter.seek_for_prev(encode_key(PartitionChunkOffset(start)));
            if iter.key().is_none() {
                iter.seek(encode_key(PartitionChunkOffset(start)));
            }
            while let Some(key) = iter.key() {
                let key_owned = key.to_vec();
                let value_owned = iter.value().unwrap_or(&[]).to_vec();
                let Ok(offset) = PartitionChunkOffset::decode(&key_owned) else {
                    iter.next();
                    continue;
                };
                if offset.0 > end {
                    break;
                }
                let hidden = self.overlay.is_some_and(|overlay| {
                    overlay.maps[index].contains_key(key_owned.as_slice())
                        || offset_key_in_ranges(&overlay.deleted_ranges[index], &key_owned)
                });
                if !hidden {
                    let record = records::decode_placement(&value_owned)?;
                    if offset.0 <= end && record.end >= start {
                        iter.status().wrap_err("scan interval overlap")?;
                        return Ok(true);
                    }
                    if record.end < start && offset.0 < start {
                        iter.seek(encode_key(PartitionChunkOffset(start)));
                        continue;
                    }
                }
                iter.next();
            }
        }
        iter.status().wrap_err("scan interval overlap")?;
        Ok(false)
    }

    /// Placement rows that overlap a span or touch a neighbor, one iterator.
    fn placements_touching_spans(
        &self,
        spans: &[(u32, u32)],
    ) -> eyre::Result<Vec<records::PlacementSpan>> {
        if spans.is_empty() {
            return Ok(Vec::new());
        }
        let index = Cf::Interval.index();
        let mut rows: BTreeMap<u32, records::PlacementRecord> = BTreeMap::new();
        let cleared = self.overlay.is_some_and(|overlay| overlay.cleared[index]);
        if !cleared {
            let handle = cf_handle(self.db, Cf::Interval)?;
            let last = spans[spans.len() - 1].1;
            let upper = match last.checked_add(1) {
                Some(next) => exclusive_end_key(PartitionChunkOffset(next)),
                None => exclusive_end_key(PartitionChunkOffset(u32::MAX)),
            };
            let mut opts = full_scan_options();
            opts.set_iterate_upper_bound(upper);
            let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
            for &(start, end) in spans {
                let right = end.saturating_add(1);
                iter.seek_for_prev(encode_key(PartitionChunkOffset(start)));
                if iter.key().is_none() {
                    iter.seek(encode_key(PartitionChunkOffset(start)));
                }
                while let Some(key) = iter.key() {
                    let key_owned = key.to_vec();
                    let value_owned = iter.value().unwrap_or(&[]).to_vec();
                    let Ok(offset) = PartitionChunkOffset::decode(&key_owned) else {
                        iter.next();
                        continue;
                    };
                    if offset.0 > right {
                        break;
                    }
                    let hidden = self.overlay.is_some_and(|overlay| {
                        overlay.maps[index].contains_key(key_owned.as_slice())
                            || offset_key_in_ranges(&overlay.deleted_ranges[index], &key_owned)
                    });
                    if !hidden {
                        let value = records::decode_placement(&value_owned)?;
                        if value.end.saturating_add(1) < start && offset.0 < start {
                            iter.seek(encode_key(PartitionChunkOffset(start)));
                            continue;
                        }
                        if offset.0 <= right && value.end.saturating_add(1) >= start {
                            rows.insert(offset.0, value);
                        }
                    }
                    if offset.0 >= right {
                        break;
                    }
                    iter.next();
                }
            }
            iter.status().wrap_err("scan placement spans")?;
        }
        if let Some(overlay) = self.overlay {
            for (key, value) in &overlay.maps[index] {
                let Ok(offset) = PartitionChunkOffset::decode(key) else {
                    continue;
                };
                match value {
                    None => {
                        rows.remove(&offset.0);
                    }
                    Some(bytes) => {
                        let record = records::decode_placement(bytes)?;
                        if row_touches_spans(spans, offset.0, record.end) {
                            rows.insert(offset.0, record);
                        }
                    }
                }
            }
        }
        Ok(rows
            .into_iter()
            .map(|(start, value)| records::PlacementSpan { start, value })
            .collect())
    }

    fn data_root_rows(
        &self,
        data_root: &H256,
    ) -> eyre::Result<Vec<(u32, records::DataRootPlacement)>> {
        let prefix = data_root.0;
        let raw = self.collect_raw(Cf::DataRoots, Some(&prefix), |key| {
            Ok(key.len() < 32 || key[..32] != prefix)
        })?;
        let mut rows = Vec::new();
        for (key, value) in raw {
            let (root, start) = records::decode_data_root_key(&key)?;
            if root != *data_root {
                break;
            }
            rows.push((start, records::decode_data_root(&value)?));
        }
        Ok(rows)
    }

    fn collect_raw(
        &self,
        cf: Cf,
        start: Option<&[u8]>,
        mut stop: impl FnMut(&[u8]) -> eyre::Result<bool>,
    ) -> eyre::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut cursor = self.cursor(cf, start)?;
        let mut rows = Vec::new();
        while let Some((key, value)) = cursor.next_kv()? {
            if stop(&key)? {
                break;
            }
            rows.push((key, value));
        }
        Ok(rows)
    }

    fn placement_row(&self, start: u32, value: &records::PlacementRecord) -> Placement {
        use super::tables::Placement;
        Placement {
            data_root: H256(value.data_root),
            tx_start_offset: irys_types::RelativeChunkOffset(value.tx_start),
            data_size: value.data_size,
            ledger: records::optional_u32(value.ledger),
            start: PartitionChunkOffset(start),
            end: PartitionChunkOffset(value.end),
            block_height: records::optional_u64(value.block_height),
            tx_index: records::optional_u32(value.tx_index),
        }
    }

    fn placement_row_at(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<super::tables::Placement>> {
        let Some(row) = self.interval_covering(offset)? else {
            return Ok(None);
        };
        let Some(bytes) = self.get_raw(Cf::Interval, &encode_key(row.start))? else {
            return Ok(None);
        };
        let value = records::decode_placement(&bytes)?;
        Ok(Some(self.placement_row(row.start.0, &value)))
    }

    fn placement_rows_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<super::tables::Placement>> {
        if start > end {
            return Ok(Vec::new());
        }
        let intervals = self.placement_intervals_touching(start, end)?;
        Ok(intervals
            .into_iter()
            .filter(|row| row.start <= end.0 && row.value.end >= start.0)
            .map(|row| self.placement_row(row.start, &row.value))
            .collect())
    }

    fn placements_for_root(
        &self,
        data_root: DataRoot,
    ) -> eyre::Result<Vec<super::tables::Placement>> {
        let mut out = Vec::new();
        for (start, _) in self.data_root_rows(&data_root)? {
            if let Some(bytes) =
                self.get_raw(Cf::Interval, &encode_key(PartitionChunkOffset(start)))?
                && let Ok(value) = records::decode_placement(&bytes)
                && value.data_root == data_root.0
            {
                out.push(self.placement_row(start, &value));
            }
        }
        out.sort_by_key(|placement| placement.start);
        Ok(out)
    }

    fn data_root_info_rows(&self, data_root: DataRoot) -> eyre::Result<Option<DataRootInfos>> {
        let rows = self.data_root_rows(&data_root)?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut infos: Vec<DataRootInfo> = rows
            .into_iter()
            .map(|(_, point)| DataRootInfo {
                start_offset: irys_types::RelativeChunkOffset(point.tx_start),
                data_size: point.data_size,
            })
            .collect();
        infos.sort_unstable();
        infos.dedup();
        Ok(Some(DataRootInfos(infos)))
    }

    fn placement_data_root_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<H256>> {
        let Some(row) = self.interval_covering(offset)? else {
            return Ok(None);
        };
        Ok((row.tx_path_hash != H256::zero()).then_some(row.tx_path_hash))
    }

    fn path_hashes_from_row(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkPathHashes>> {
        // Hash the proof only. A checksum-only row is not a data chunk, and
        // the trailer bytes are not part of the proof.
        let data_path_hash = self
            .get_offset_meta(offset)?
            .and_then(|meta| meta.proof)
            .map(|proof| H256(irys_types::hash_sha256(&proof)));
        let tx_path_hash = self.placement_data_root_at(offset)?;
        Ok(combine_hashes(
            data_path_hash.map(|hash| ChunkPathHashes {
                data_path_hash: Some(hash),
                tx_path_hash: None,
            }),
            tx_path_hash,
        ))
    }

    fn path_hashes_in_span(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        let intervals = self.intervals_touching(start, end)?;
        let start_key = encode_key(start);
        let stored = self.collect_raw(Cf::OffsetMeta, Some(&start_key), |key| {
            offset_key_ends_span(key, end)
        })?;
        let mut index = 0;
        let mut rows = Vec::new();
        for (key, bytes) in stored {
            let offset = PartitionChunkOffset::decode(&key)?;
            let meta = stored_offset_meta(&bytes)?;
            let Some(proof) = meta.proof else {
                continue;
            };
            while index < intervals.len() && intervals[index].end < offset {
                index += 1;
            }
            let tx_path_hash = intervals.get(index).and_then(|interval| {
                (interval.start <= offset && interval.end >= offset)
                    .then_some(interval.tx_path_hash)
            });
            let tx_path_hash = tx_path_hash.filter(|hash| *hash != H256::zero());
            rows.push((
                offset,
                ChunkPathHashes {
                    data_path_hash: Some(H256(irys_types::hash_sha256(&proof))),
                    tx_path_hash,
                },
            ));
        }
        Ok(rows)
    }

    fn pending_rows_from(
        &self,
        start: Option<PartitionChunkOffset>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        let start_key = start.map(encode_key);
        let mut remaining = limit.unwrap_or(usize::MAX);
        let raw = self.collect_raw(Cf::Pending, start_key.as_deref(), |_| {
            if remaining == 0 {
                return Ok(true);
            }
            remaining -= 1;
            Ok(false)
        })?;
        let mut rows = Vec::new();
        for (key, value) in raw {
            rows.push((
                PartitionChunkOffset::decode(&key)?,
                records::decode_pending(&value)?,
            ));
        }
        Ok(rows)
    }

    fn all_data_root_rows(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        let raw = self.collect_raw(Cf::DataRoots, None, |_| Ok(false))?;
        let mut grouped: BTreeMap<H256, Vec<DataRootInfo>> = BTreeMap::new();
        for (key, value) in raw {
            let (root, _) = records::decode_data_root_key(&key)?;
            let point = records::decode_data_root(&value)?;
            grouped.entry(root).or_default().push(DataRootInfo {
                start_offset: irys_types::RelativeChunkOffset(point.tx_start),
                data_size: point.data_size,
            });
        }
        Ok(grouped
            .into_iter()
            .map(|(root, mut infos)| {
                infos.sort_unstable();
                infos.dedup();
                (root, DataRootInfos(infos))
            })
            .collect())
    }
}

fn overlay_pairs(
    overlay: &Overlay,
    cf: Cf,
    start: Option<&[u8]>,
    upper: Option<&[u8]>,
) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let map = &overlay.maps[cf.index()];
    let mut out = Vec::new();
    if let Some(start) = start {
        let start = start.to_vec();
        for (key, value) in map.range(start..) {
            if upper.is_some_and(|end| key.as_slice() >= end) {
                break;
            }
            out.push((key.clone(), value.clone()));
        }
    } else {
        for (key, value) in map.iter() {
            if upper.is_some_and(|end| key.as_slice() >= end) {
                break;
            }
            out.push((key.clone(), value.clone()));
        }
    }
    out
}

struct MergeCursor<'a> {
    db: Box<dyn Iterator<Item = eyre::Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    db_next: Option<(Vec<u8>, Vec<u8>)>,
    overlay: std::vec::IntoIter<(Vec<u8>, Option<Vec<u8>>)>,
    ov_next: Option<(Vec<u8>, Option<Vec<u8>>)>,
    cleared: bool,
    failed: Option<eyre::Report>,
}

impl<'a> MergeCursor<'a> {
    fn new(
        db: Box<dyn Iterator<Item = eyre::Result<(Vec<u8>, Vec<u8>)>> + 'a>,
        overlay: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        cleared: bool,
    ) -> Self {
        let mut cursor = Self {
            db,
            db_next: None,
            overlay: overlay.into_iter(),
            ov_next: None,
            cleared,
            failed: None,
        };
        cursor.pump_db();
        cursor.pump_overlay();
        cursor
    }

    fn pump_db(&mut self) {
        if self.cleared || self.failed.is_some() {
            self.db_next = None;
            return;
        }
        match self.db.next() {
            Some(Ok(kv)) => self.db_next = Some(kv),
            Some(Err(err)) => {
                self.failed = Some(err);
                self.db_next = None;
            }
            None => self.db_next = None,
        }
    }

    fn pump_overlay(&mut self) {
        self.ov_next = self.overlay.next();
    }

    fn next_kv(&mut self) -> eyre::Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(err) = self.failed.take() {
                return Err(err);
            }
            let side = match (
                self.db_next.as_ref().map(|(key, _)| key.as_slice()),
                self.ov_next.as_ref().map(|(key, _)| key.as_slice()),
            ) {
                (None, None) => return Ok(None),
                (Some(_), None) => Side::Db,
                (None, Some(_)) => Side::Overlay,
                (Some(db_key), Some(ov_key)) => match ov_key.cmp(db_key) {
                    std::cmp::Ordering::Less => Side::Overlay,
                    std::cmp::Ordering::Equal => Side::Both,
                    std::cmp::Ordering::Greater => Side::Db,
                },
            };
            match side {
                Side::Db => {
                    let kv = self.db_next.take().expect("db key is pending");
                    self.pump_db();
                    return Ok(Some(kv));
                }
                Side::Overlay => {
                    let (key, value) = self.ov_next.take().expect("overlay key is pending");
                    self.pump_overlay();
                    if let Some(value) = value {
                        return Ok(Some((key, value)));
                    }
                }
                Side::Both => {
                    self.db_next.take();
                    self.pump_db();
                    let (key, value) = self.ov_next.take().expect("overlay key is pending");
                    self.pump_overlay();
                    if let Some(value) = value {
                        return Ok(Some((key, value)));
                    }
                }
            }
        }
    }
}

enum Side {
    Db,
    Overlay,
    Both,
}

impl SubmoduleRead for Rows<'_> {
    fn stores_tx_paths(&self) -> bool {
        false
    }

    fn stores_offset_crc(&self) -> bool {
        true
    }

    fn get_data_path_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkDataPath>> {
        Ok(self.get_offset_meta(offset)?.and_then(|meta| meta.proof))
    }

    fn get_offset_meta(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<OffsetMeta>> {
        let key = encode_key(offset);
        let Some(bytes) = self.get_raw(Cf::OffsetMeta, &key)? else {
            return Ok(None);
        };
        stored_offset_meta(&bytes).map(Some)
    }

    fn offset_metas_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, OffsetMeta)>> {
        if self.layout != IndexLayout::RocksV1 || start > end {
            return Ok(Vec::new());
        }
        let start_key = encode_key(start);
        let stored = self.collect_raw(Cf::OffsetMeta, Some(&start_key), |key| {
            offset_key_ends_span(key, end)
        })?;
        let mut rows = Vec::with_capacity(stored.len());
        for (key, bytes) in stored {
            let offset = PartitionChunkOffset::decode(&key)?;
            rows.push((offset, stored_offset_meta(&bytes)?));
        }
        Ok(rows)
    }

    fn offset_crc_missing(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        if start > end {
            return Ok(false);
        }
        // Streams: a partition holds tens of millions of rows. The exclusive
        // end keeps a key past the span out of the iterator. A key that is
        // not 4 bytes and still sorts inside the span fails the scan.
        let start_key = encode_key(start);
        let end_key = exclusive_end_key(end);
        let mut cursor = self.cursor_bounded(Cf::OffsetMeta, Some(&start_key), Some(&end_key))?;
        let mut expect = u64::from(start.0);
        let last = u64::from(end.0);
        while let Some((key, bytes)) = cursor.next_kv()? {
            if offset_key_ends_span(&key, end)? {
                break;
            }
            let offset = u64::from(PartitionChunkOffset::decode(&key)?.0);
            if offset != expect || stored_offset_meta(&bytes)?.crc.is_none() {
                return Ok(true);
            }
            expect = expect.saturating_add(1);
        }
        Ok(expect != last.saturating_add(1))
    }

    fn get_tx_path_by_offset(&self, _offset: PartitionChunkOffset) -> eyre::Result<Option<TxPath>> {
        Ok(None)
    }

    fn get_path_hashes_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkPathHashes>> {
        self.path_hashes_from_row(offset)
    }

    fn path_hashes_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        if start > end {
            return Ok(Vec::new());
        }

        self.path_hashes_in_span(start, end)
    }

    fn first_missing_path_hash_offset(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Option<PartitionChunkOffset>> {
        Ok(self
            .missing_path_hash_ranges(start, end)?
            .into_iter()
            .next()
            .map(|(gap_start, _)| gap_start))
    }

    fn missing_path_hash_ranges(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
        if start >= end {
            return Ok(Vec::new());
        }
        let intervals = self.intervals_touching(start, PartitionChunkOffset(end.0 - 1))?;
        Ok(coverage_gaps(start, end, &intervals))
    }

    fn get_full_tx_path(&self, _path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
        Ok(None)
    }

    fn get_tx_leaf_binding(&self, _path_hash: TxPathHash) -> eyre::Result<Option<TxLeafBinding>> {
        Ok(None)
    }

    fn get_data_root_infos_for_data_root(
        &self,
        data_root: DataRoot,
    ) -> eyre::Result<Option<DataRootInfos>> {
        self.data_root_info_rows(data_root)
    }

    fn get_pending_body_migration(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<PendingBodyMigration>> {
        let key = encode_key(offset);

        Ok(match self.get_raw(Cf::Pending, &key)? {
            Some(bytes) => Some(records::decode_pending(&bytes)?),
            None => None,
        })
    }

    fn pending_body_migrations_from(
        &self,
        start: Option<PartitionChunkOffset>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        self.pending_rows_from(start, limit)
    }

    fn has_pending_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        self.offset_range_has_key(Cf::Pending, start, end)
    }

    fn tx_interval_overlaps(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        if start > end {
            return Ok(false);
        }
        let rows = self.intervals_touching(start, end)?;
        Ok(rows.iter().any(|row| row.start <= end && row.end >= start))
    }

    fn has_pending_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<bool> {
        self.pending_hit_in_spans(&coalesced_u32_spans(ranges))
    }

    fn tx_intervals_overlap_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<bool> {
        self.intervals_overlap_u32_spans(&coalesced_u32_spans(ranges))
    }

    fn tx_interval_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<IntervalRow>> {
        self.interval_covering(offset)
    }

    fn tx_intervals_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<IntervalRow>> {
        self.intervals_overlapping(start, end)
    }

    fn placement_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<Placement>> {
        self.placement_row_at(offset)
    }

    fn placements_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<Placement>> {
        self.placement_rows_in_range(start, end)
    }

    fn placements_for_data_root(&self, data_root: DataRoot) -> eyre::Result<Vec<Placement>> {
        self.placements_for_root(data_root)
    }

    fn offsets_for_content_hash(
        &self,
        content_hash: H256,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let prefix = content_hash.0;
        let mut found = BTreeSet::new();
        let cleared = self
            .overlay
            .is_some_and(|overlay| overlay.cleared[Cf::Content.index()]);
        if !cleared {
            let handle = cf_handle(self.db, Cf::Content)?;
            let mut opts = ReadOptions::default();
            // Prefix mode consults the prefix bloom. `total_order_seek` stays
            // off. `prefix_same_as_start` stops the iterator at the next hash.
            opts.set_prefix_same_as_start(true);
            opts.fill_cache(true);
            let mut iter = self.snap.raw_iterator_cf_opt(&handle, opts);
            iter.seek(prefix.as_slice());
            while let Some(key) = iter.key() {
                if !key.starts_with(&prefix) {
                    break;
                }
                let key_owned = key.to_vec();
                let hidden = self.overlay.is_some_and(|overlay| {
                    matches!(
                        overlay.maps[Cf::Content.index()].get(key_owned.as_slice()),
                        Some(None)
                    )
                });
                if !hidden
                    && let Some((hash, offset)) = records::decode_content_key(&key_owned)
                    && hash == content_hash
                {
                    found.insert(offset);
                }
                iter.next();
            }
            iter.status().wrap_err("seek content-hash offset index")?;
        }
        if let Some(overlay) = self.overlay {
            let start = prefix.to_vec();
            for (key, value) in overlay.maps[Cf::Content.index()].range(start..) {
                if !key.starts_with(&prefix) {
                    break;
                }
                let Some((hash, offset)) = records::decode_content_key(key) else {
                    continue;
                };
                if hash != content_hash {
                    continue;
                }
                match value {
                    Some(_) => {
                        found.insert(offset);
                    }
                    None => {
                        found.remove(&offset);
                    }
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        self.all_data_root_rows()
    }
}

/// `Batch` reads through its overlay. Two interval queries name the row helper.
macro_rules! impl_batch_read {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        impl SubmoduleRead for Batch<'_> {
            $(
                fn $name(&self, $($arg: $ty),*) -> $ret {
                    self.rows().$name($($arg),*)
                }
            )*

            fn tx_interval_at(
                &self,
                offset: PartitionChunkOffset,
            ) -> eyre::Result<Option<IntervalRow>> {
                self.rows().interval_covering(offset)
            }

            fn tx_intervals_in_range(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<Vec<IntervalRow>> {
                self.rows().intervals_overlapping(start, end)
            }
        }
    };
}

impl_batch_read! {
    stores_tx_paths() -> bool;
    stores_offset_crc() -> bool;
    get_data_path_by_offset(offset: PartitionChunkOffset) -> eyre::Result<Option<ChunkDataPath>>;
    get_offset_meta(offset: PartitionChunkOffset) -> eyre::Result<Option<OffsetMeta>>;
    offset_metas_in_range(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<Vec<(PartitionChunkOffset, OffsetMeta)>>;
    offset_crc_missing(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<bool>;
    get_tx_path_by_offset(offset: PartitionChunkOffset) -> eyre::Result<Option<TxPath>>;
    get_path_hashes_by_offset(offset: PartitionChunkOffset) -> eyre::Result<Option<ChunkPathHashes>>;
    path_hashes_in_inclusive_range(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>>;
    first_missing_path_hash_offset(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<Option<PartitionChunkOffset>>;
    missing_path_hash_ranges(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>>;
    get_full_tx_path(path_hash: TxPathHash) -> eyre::Result<Option<TxPath>>;
    get_tx_leaf_binding(path_hash: TxPathHash) -> eyre::Result<Option<TxLeafBinding>>;
    get_data_root_infos_for_data_root(data_root: DataRoot) -> eyre::Result<Option<DataRootInfos>>;
    get_pending_body_migration(offset: PartitionChunkOffset) -> eyre::Result<Option<PendingBodyMigration>>;
    pending_body_migrations_from(start: Option<PartitionChunkOffset>, limit: Option<usize>) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>>;
    has_pending_in_inclusive_range(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<bool>;
    tx_interval_overlaps(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<bool>;
    has_pending_in_ranges(ranges: &[(PartitionChunkOffset, PartitionChunkOffset)]) -> eyre::Result<bool>;
    tx_intervals_overlap_ranges(ranges: &[(PartitionChunkOffset, PartitionChunkOffset)]) -> eyre::Result<bool>;
    data_root_entries() -> eyre::Result<Vec<(DataRoot, DataRootInfos)>>;
    placement_at(offset: PartitionChunkOffset) -> eyre::Result<Option<Placement>>;
    placements_in_range(start: PartitionChunkOffset, end: PartitionChunkOffset) -> eyre::Result<Vec<Placement>>;
    placements_for_data_root(data_root: DataRoot) -> eyre::Result<Vec<Placement>>;
    offsets_for_content_hash(content_hash: H256) -> eyre::Result<Vec<PartitionChunkOffset>>;
}

fn warn_uncovered_proof(offset: PartitionChunkOffset) {
    tracing::warn!(
        offset = offset.0,
        "data path has no covering interval; the proof was not stored"
    );
}

impl SubmoduleWrite for Batch<'_> {
    fn add_full_data_path(
        &mut self,
        offset: PartitionChunkOffset,
        data_path: ChunkDataPath,
    ) -> eyre::Result<()> {
        let key = encode_key(offset);

        // A proof committed after its interval was cleared would outlive
        // the placement. Drop it. The batch overlay is visible here.
        if self.rows().interval_covering(offset)?.is_none() {
            warn_uncovered_proof(offset);
            return Ok(());
        }
        // A proof rewrite keeps the checksum already stored for these
        // bytes. The durable-point write replaces both together.
        let old_crc = self
            .rows()
            .get_raw(Cf::OffsetMeta, &key)?
            .as_deref()
            .map(stored_offset_meta)
            .transpose()?
            .and_then(|meta| meta.crc);
        self.put_offset_record(
            offset,
            OffsetMeta {
                crc: old_crc,
                proof: Some(data_path),
            },
        )?;

        Ok(())
    }

    fn add_full_tx_path(&mut self, _path_hash: TxPathHash, _tx_path: TxPath) -> eyre::Result<()> {
        // RocksDB v1 stores no tx path. The placement data root is the binding.
        Ok(())
    }

    fn add_tx_leaf_binding(
        &mut self,
        path_hash: TxPathHash,
        binding: &TxLeafBinding,
    ) -> eyre::Result<()> {
        // The interval row is written later in this batch and reads the root
        // from this map. No interval exists yet, so nothing is rewritten.
        self.bindings.insert(path_hash, binding.clone());
        Ok(())
    }

    fn add_data_path_hash_to_offset_index(
        &mut self,
        _offset: PartitionChunkOffset,
        _path_hash: Option<ChunkPathHash>,
    ) -> eyre::Result<()> {
        // RocksDB v1 has no path-hash family. A proof in the offset row is the
        // presence bit. A checksum-only row is not.
        Ok(())
    }

    fn add_tx_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        self.assign_placement(offset, offset, path_hash)
    }

    fn add_tx_path_hash_to_offset_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        self.assign_placement(start, end, path_hash)
    }

    fn write_data_path_updates(
        &mut self,
        mut updates: Vec<(PartitionChunkOffset, ChunkPathHash, ChunkDataPath)>,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        updates.sort_by_key(|(offset, _, _)| *offset);
        let mut uncovered = Vec::new();
        for (offset, _path_hash, data_path) in updates {
            // The hash is not stored. A proof with no interval is dropped.
            // `add_full_data_path` would warn again for the same offset.
            if self.layout == IndexLayout::RocksV1
                && self.rows().interval_covering(offset)?.is_none()
            {
                warn_uncovered_proof(offset);
                uncovered.push(offset);
                continue;
            }
            self.add_full_data_path(offset, data_path)?;
        }
        Ok(uncovered)
    }

    fn write_packed_offset_rows(
        &mut self,
        rows: Vec<PackedOffsetRow>,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let mut uncovered = Vec::new();
        for row in rows {
            if row.proof.is_some() && self.rows().interval_covering(row.offset)?.is_none() {
                warn_uncovered_proof(row.offset);
                uncovered.push(row.offset);
                continue;
            }
            self.put_offset_record(
                row.offset,
                OffsetMeta {
                    crc: row.crc,
                    proof: row.proof,
                },
            )?;
        }
        Ok(uncovered)
    }

    fn set_path_hashes_by_offset(
        &mut self,
        offset: PartitionChunkOffset,
        path_hashes: ChunkPathHashes,
    ) -> eyre::Result<()> {
        // The data-path hash has no family. The tx hash is the placement.
        self.assign_placement(offset, offset, path_hashes.tx_path_hash)
    }

    fn del_path_hashes_by_offset(&mut self, offset: PartitionChunkOffset) -> eyre::Result<()> {
        self.clear_paths_in_inclusive_range(offset, offset)
    }

    fn clear_paths_in_inclusive_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        if start > end {
            return Ok(());
        }
        self.assign_placement(start, end, None)?;

        // `delete_offset_range` hides these keys from a later read.
        // Pending stays. The caller purges it in its own commit.
        self.delete_content_rows_for_paths(start, end)?;

        self.delete_offset_range(Cf::OffsetMeta, start, end)?;
        Ok(())
    }

    fn set_data_root_infos_for_data_root(
        &mut self,
        data_root: DataRoot,
        infos: DataRootInfos,
    ) -> eyre::Result<()> {
        self.replace_infos(data_root, &infos)
    }

    fn add_data_root_info_at(
        &mut self,
        start: PartitionChunkOffset,
        data_root: DataRoot,
        info: &DataRootInfo,
    ) -> eyre::Result<()> {
        let key = encode_key(start);
        let Some(bytes) = self.rows().get_raw(Cf::Interval, &key)? else {
            eyre::bail!("RocksDB v1 data-root info at {} has no interval", start.0);
        };
        let value = records::decode_placement(&bytes)?;
        if value.data_root != data_root.0 {
            eyre::bail!(
                "RocksDB v1 interval at {} belongs to a different data root",
                start.0
            );
        }
        self.fill_info_at(start.0, info)
    }

    fn intervals_own_data_root_rows(&self) -> bool {
        self.layout == IndexLayout::RocksV1
    }

    fn add_data_root_info(&mut self, data_root: DataRoot, info: &DataRootInfo) -> eyre::Result<()> {
        self.fill_info(data_root, info)
    }

    fn add_pending_body_migration(
        &mut self,
        offset: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> eyre::Result<()> {
        let key = encode_key(offset);

        self.put_bytes(Cf::Pending, &key, records::encode_pending(job).to_vec());

        Ok(())
    }

    fn del_pending_body_migration(&mut self, offset: PartitionChunkOffset) -> eyre::Result<bool> {
        let existed = self.rows().get_pending_body_migration(offset)?.is_some();
        if existed {
            self.delete(Cf::Pending, &encode_key(offset));
        }
        Ok(existed)
    }

    fn del_pending_body_migrations_in_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<usize> {
        // The key is the offset. One `DeleteRange` covers `[start, end]`.
        // The count is keys only. The job value is not read.
        let removed = self.rows().count_offset_keys(Cf::Pending, start, end)?;
        if removed > 0 {
            self.delete_offset_range(Cf::Pending, start, end)?;
        }
        Ok(removed)
    }

    fn del_pending_body_migrations_in_ranges(
        &mut self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<usize> {
        self.delete_offset_spans(Cf::Pending, ranges)
    }

    fn clear_tx_coverage_in_ranges(
        &mut self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<()> {
        self.clear_placement_spans(ranges)
    }

    fn clear(&mut self) -> eyre::Result<()> {
        for cf in families() {
            if matches!(cf, Cf::Metadata) {
                continue;
            }
            self.overlay.cleared[cf.index()] = true;
            self.overlay.maps[cf.index()].clear();
            self.overlay.deleted_ranges[cf.index()].clear();
        }

        // The next assignment may name a different partition. The packing
        // record stays: reset does not change the miner or the chain.
        self.delete(Cf::Metadata, &encode_key(MetadataKey::PartitionHash));

        Ok(())
    }

    fn note_placement_origin(
        &mut self,
        ledger: u32,
        block_height: u64,
        tx_index: u32,
    ) -> eyre::Result<()> {
        self.origin = Some(records::PlacementOrigin {
            ledger,
            block_height,
            tx_index,
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submodule::SubmoduleIndex;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, RelativeChunkOffset};

    fn temp_dir(prefix: &str) -> irys_testing_utils::utils::tempfile::TempDir {
        TempDirBuilder::new().prefix(prefix).build()
    }

    #[test]
    fn offset_range_membership_is_logarithmic() {
        fn linear(ranges: &[(u32, u32)], offset: u32) -> bool {
            ranges
                .iter()
                .any(|(start, end)| offset >= *start && offset <= *end)
        }
        let small_n = 4_000_u32;
        let small: Vec<(u32, u32)> = (0..small_n).map(|i| (i, i)).collect();
        let started = Instant::now();
        let mut linear_hits = 0_usize;
        for _ in 0..small_n {
            if linear(&small, small_n - 1) {
                linear_hits += 1;
            }
        }
        let linear_elapsed = started.elapsed();
        eprintln!(
            "linear membership worst-case n={small_n} probes={small_n} took {linear_elapsed:?} hits={linear_hits}"
        );

        let n = 100_000_u32;
        let ranges: Vec<(u32, u32)> = (0..n).map(|i| (i, i)).collect();
        let started = Instant::now();
        let mut hits = 0_usize;
        let last = encode_key(PartitionChunkOffset(n - 1));
        for probe in 0..n {
            if offset_key_in_ranges(&ranges, &last) {
                hits += 1;
            }
            if probe == 499 && started.elapsed() > Duration::from_millis(250) {
                panic!(
                    "membership still linear after {} probes: {:?}",
                    probe + 1,
                    started.elapsed()
                );
            }
        }
        let elapsed = started.elapsed();
        eprintln!("offset_key_in_ranges worst-case n={n} probes={n} took {elapsed:?} hits={hits}");
        assert_eq!(hits, n as usize);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn deleted_ranges_merge_touching_spans() {
        let mut ranges = Vec::new();
        insert_deleted_range(&mut ranges, 0, 1);
        insert_deleted_range(&mut ranges, 3, 4);
        insert_deleted_range(&mut ranges, 2, 2);
        assert_eq!(ranges, vec![(0, 4)]);
        insert_deleted_range(&mut ranges, 8, 8);
        insert_deleted_range(&mut ranges, 6, 6);
        assert_eq!(ranges, vec![(0, 4), (6, 6), (8, 8)]);
        assert!(offset_key_in_ranges(
            &ranges,
            &encode_key(PartitionChunkOffset(6))
        ));
        assert!(!offset_key_in_ranges(
            &ranges,
            &encode_key(PartitionChunkOffset(5))
        ));
        assert!(!offset_key_in_ranges(
            &ranges,
            &encode_key(PartitionChunkOffset(7))
        ));
        assert!(!offset_key_in_ranges(&ranges, b"not-an-offset"));
    }

    #[test]
    fn many_disjoint_spans_share_one_pending_pass() -> eyre::Result<()> {
        let n = 2_000_u32;
        let dir = temp_dir("submodule_span_pass");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        store.disable_auto_compaction()?;
        let job = PendingBodyMigration {
            data_root: H256::repeat_byte(1),
            data_size: 32,
            start_offset: RelativeChunkOffset(0),
            block_height: 1,
            attempts: 0,
        };
        let survivor = PartitionChunkOffset::from(n * 2 + 3);
        store.update(|tx| {
            tx.add_pending_body_migration(survivor, &job)?;
            for i in 0..n {
                tx.add_pending_body_migration(PartitionChunkOffset::from(i * 2), &job)?;
            }
            Ok(())
        })?;
        let plant = Instant::now();
        store.update(|tx| {
            for i in 0..n {
                let offset = PartitionChunkOffset::from(i * 2);
                tx.del_pending_body_migrations_in_range(offset, offset)?;
            }
            Ok(())
        })?;
        let plant_elapsed = plant.elapsed();
        eprintln!("plant {n} single-key range tombstones took {plant_elapsed:?}");
        store.flush_memtables()?;
        assert!(
            store
                .view(|tx| tx.get_pending_body_migration(survivor))?
                .is_some(),
            "a key outside every span stays"
        );

        let misses: Vec<_> = (0..n)
            .map(|i| {
                let offset = PartitionChunkOffset::from(i * 2 + 1);
                (offset, offset)
            })
            .collect();
        let started = Instant::now();
        let hit = store.view(|tx| tx.has_pending_in_ranges(&misses))?;
        let batched = started.elapsed();
        eprintln!(
            "has_pending_in_ranges {n} misses over {n} tombstones took {batched:?} hit={hit}"
        );
        assert!(!hit);
        assert!(batched < Duration::from_secs(8), "{batched:?}");

        let sample = 400_u32;
        let started = Instant::now();
        let mut seen = 0_u32;
        store.view(|tx| {
            for i in 0..sample {
                let offset = PartitionChunkOffset::from(i * 2 + 1);
                if tx.has_pending_in_inclusive_range(offset, offset)? {
                    seen = seen.saturating_add(1);
                }
            }
            Ok(())
        })?;
        let per_span = started.elapsed();
        eprintln!(
            "has_pending_in_inclusive_range x{sample} over {n} tombstones took {per_span:?} hits={seen}"
        );
        assert_eq!(seen, 0);
        Ok(())
    }

    #[test]
    fn reopen_reads_the_row_and_keeps_the_schema_across_clear() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_reopen");
        let path = dir.path().join("index");
        let offset = PartitionChunkOffset::from(3);
        let root = H256::repeat_byte(2);
        {
            let store = RocksSubmoduleStore::open(&path)?;
            place_span(&store, 0, 7, root, 0, 1, 0, &[3], None)?;
            assert_eq!(
                store
                    .view(|tx| tx.get_path_hashes_by_offset(offset))?
                    .and_then(|hashes| hashes.tx_path_hash),
                Some(root)
            );
        }
        {
            let store = RocksSubmoduleStore::open(&path)?;
            store.update(|tx| tx.clear())?;
            let payload = store.schema_bytes()?.expect("schema row");
            assert_eq!(payload, SUBMODULE_SCHEMA_ROCKS_V1.as_bytes());
            assert!(
                store
                    .view(|tx| tx.get_path_hashes_by_offset(offset))?
                    .is_none()
            );
        }
        let store = SubmoduleIndex::open_rocks(&path)?;
        assert!(
            store
                .view(|tx| tx.get_path_hashes_by_offset(offset))?
                .is_none()
        );
        let marker = fs::read_to_string(path.join(SCHEMA_FILE))?;
        assert_eq!(marker, SUBMODULE_SCHEMA_ROCKS_V1);
        Ok(())
    }

    /// One process must not open the same directory twice. `fcntl` on `LOCK`
    /// does not stop a second open in this process, and the live handle's
    /// compaction deletes SST files the second open has already read from
    /// the manifest.
    #[test]
    fn second_open_of_a_live_directory_is_refused() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_one_open");
        let path = dir.path().join("index");
        let writer = RocksSubmoduleStore::open(&path)?;
        let read_only = RocksSubmoduleStore::open_read_only(&path).unwrap_err();
        assert_live_open_refused(&read_only);
        let writable = RocksSubmoduleStore::open(&path).unwrap_err();
        assert_live_open_refused(&writable);
        // A clone is the same handle. Dropping one side must not allow a
        // second open while the other side is still live.
        let cloned = writer.clone();
        drop(writer);
        let still_live = RocksSubmoduleStore::open_read_only(&path).unwrap_err();
        assert_live_open_refused(&still_live);
        drop(cloned);
        let other = dir.path().join("other");
        let second = RocksSubmoduleStore::open(&other)?;
        drop(second);
        let reader = RocksSubmoduleStore::open_read_only(&path)?;
        let payload = reader.schema_bytes()?.expect("schema row");
        assert_eq!(payload, SUBMODULE_SCHEMA_ROCKS_V1.as_bytes());
        drop(reader);
        let reopened = RocksSubmoduleStore::open(&path)?;
        drop(reopened);
        Ok(())
    }

    fn assert_live_open_refused(err: &eyre::Report) {
        let text = format!("{err:#}");
        assert!(text.contains("already open in this process"), "{text}");
        assert!(!text.contains("Corruption"), "{text}");
        assert!(!text.contains("No such file"), "{text}");
    }

    fn assert_one_claim_stat(before: u64) {
        let stats = claim_directory_stats().saturating_sub(before);
        assert_eq!(
            stats, 1,
            "claim stats the candidate once, not every open directory"
        );
    }

    /// The claim key is the canonical path at open. Moving the directory
    /// must not let a second open reach RocksDB while the handle is live.
    /// After every handle drops, the new name opens and the rows are readable.
    #[test]
    fn probe_claim_rename_and_symlink_to_the_new_name() -> eyre::Result<()> {
        let dir = temp_dir("probe_claim_rename");
        let rocks = dir.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open(&rocks)?;
        store.disable_auto_compaction()?;
        for offset in 0..6 {
            put_offset(&store, offset, offset as u8)?;
            flush_every_family(&store)?;
        }
        // Same path through a symlink, before the rename. The canonical
        // path already refuses this. The rename below is a different path.
        let same = dir.path().join("same-link");
        std::os::unix::fs::symlink(&rocks, &same)?;
        assert_live_open_refused(&RocksSubmoduleStore::open_read_only(&same).unwrap_err());

        let moved = dir.path().join("rocks-moved");
        fs::rename(&rocks, &moved)?;
        let link = dir.path().join("moved-link");
        std::os::unix::fs::symlink(&moved, &link)?;
        let nested = dir.path().join("nested");
        fs::create_dir(&nested)?;
        let via_dotdot = nested.join("..").join("rocks-moved");
        let before = dir_names(&moved)?;

        // The old spelling stays claimed even though that path is gone.
        let old_spell = RocksSubmoduleStore::open(&rocks).unwrap_err();
        assert_live_open_refused(&old_spell);
        assert!(
            !rocks.join("CURRENT").exists(),
            "refused open of the old spelling must not create a database"
        );

        // Other directories stay openable. The measured claim must not
        // stat each of them.
        let mut extras = Vec::new();
        for index in 0..4 {
            extras.push(RocksSubmoduleStore::open(
                dir.path().join(format!("extra-{index}")),
            )?);
        }

        let stats_before = claim_directory_stats();
        match RocksSubmoduleStore::open_read_only(&moved) {
            Ok(reader) => {
                drop(reader);
                eyre::bail!(
                    "renamed live directory opened. stats={} before={before:?} after={:?}",
                    claim_directory_stats().saturating_sub(stats_before),
                    dir_names(&moved)?
                );
            }
            Err(err) => {
                assert_one_claim_stat(stats_before);
                assert_live_open_refused(&err);
            }
        }
        assert_eq!(dir_names(&moved)?, before);

        let stats_before = claim_directory_stats();
        match RocksSubmoduleStore::open_read_only(&link) {
            Ok(reader) => {
                drop(reader);
                eyre::bail!(
                    "symlink to the renamed live directory opened. stats={} before={before:?} after={:?}",
                    claim_directory_stats().saturating_sub(stats_before),
                    dir_names(&moved)?
                );
            }
            Err(err) => {
                assert_one_claim_stat(stats_before);
                assert_live_open_refused(&err);
            }
        }
        assert_eq!(dir_names(&moved)?, before);

        let stats_before = claim_directory_stats();
        assert_live_open_refused(
            &RocksSubmoduleStore::open_read_only(moved.join(".")).expect_err("dot path"),
        );
        assert_one_claim_stat(stats_before);
        assert_live_open_refused(
            &RocksSubmoduleStore::open_read_only(&via_dotdot).expect_err("dotdot path"),
        );
        assert_eq!(dir_names(&moved)?, before);

        let sibling = dir.path().join("sibling");
        let other = RocksSubmoduleStore::open(&sibling)?;
        put_offset(&other, 2, 2)?;
        assert!(path_hashes_at(&other, 2)?.is_some());
        drop(other);

        // Clone holds the path and the inode. Dropping one side must not
        // release the renamed directory.
        let stats_before = claim_directory_stats();
        let cloned = store.clone();
        assert_eq!(claim_directory_stats(), stats_before);
        drop(store);
        assert_live_open_refused(&RocksSubmoduleStore::open_read_only(&link).unwrap_err());
        drop(cloned);
        drop(extras);

        let reader = RocksSubmoduleStore::open_read_only(&link)?;
        for offset in 0..6 {
            assert!(
                path_hashes_at(&reader, offset)?.is_some(),
                "offset {offset} missing after the live handle dropped"
            );
        }
        drop(reader);

        // The old spelling is free. A new directory there is a different inode.
        let fresh = RocksSubmoduleStore::open(&rocks)?;
        put_offset(&fresh, 0, 1)?;
        assert!(path_hashes_at(&fresh, 0)?.is_some());
        drop(fresh);
        let again = RocksSubmoduleStore::open_read_only(&moved)?;
        for offset in 0..6 {
            assert!(
                path_hashes_at(&again, offset)?.is_some(),
                "offset {offset} missing from the renamed directory"
            );
        }
        drop(again);
        Ok(())
    }

    /// Production abandon parks the live directory and forgets the handle.
    /// The claim key stays the path from open. A symlink whose name is not
    /// the scratch name must not reach RocksDB, and the error drop must not
    /// rewrite the parked manifest. A sibling directory still opens.
    #[test]
    fn probe_claim_parked_scratch_alias() -> eyre::Result<()> {
        let dir = temp_dir("probe_claim_park");
        let rocks = dir.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        store.disable_auto_compaction()?;
        put_offset(&store, 0, 7)?;
        flush_every_family(&store)?;
        let _stop = set_close_scratch_stop(CloseScratchStop::Rename);
        let err = close_without_touching_directory(store, &rocks).unwrap_err();
        let parked_err = format!("{err:#}");
        assert!(parked_err.contains("stopped refused close"), "{parked_err}");
        let names = dir_names(dir.path())?;
        let scratch_name = names
            .iter()
            .find(|name| name.contains(".close-scratch-") && !name.ends_with("marker"))
            .cloned()
            .ok_or_else(|| eyre::eyre!("no scratch dir in {names:?}"))?;
        let scratch = dir.path().join(&scratch_name);
        let link = dir.path().join("scratch-alias");
        std::os::unix::fs::symlink(&scratch, &link)?;
        let before = dir_names(&scratch)?;

        let read_only = RocksSubmoduleStore::open_read_only(&link).unwrap_err();
        let read_only_text = format!("{read_only:#}");
        assert!(read_only_text.contains("no SCHEMA"), "{read_only_text}");
        assert!(
            !read_only_text.contains("Corruption") && !read_only_text.contains("No such file"),
            "{read_only_text}"
        );
        assert_eq!(dir_names(&scratch)?, before);

        // The forgotten claim blocks one inode. Another directory still opens.
        let sibling = dir.path().join("sibling");
        let other = RocksSubmoduleStore::open(&sibling)?;
        put_offset(&other, 2, 2)?;
        assert!(path_hashes_at(&other, 2)?.is_some());
        drop(other);
        let sibling_reader = RocksSubmoduleStore::open_read_only(&sibling)?;
        assert!(path_hashes_at(&sibling_reader, 2)?.is_some());
        drop(sibling_reader);

        let direct = RocksSubmoduleStore::open_converting(&scratch).unwrap_err();
        let direct_text = format!("{direct:#}");
        assert!(direct_text.contains("close scratch"), "{direct_text}");
        assert_eq!(dir_names(&scratch)?, before, "{direct_text}");

        let via_dot = RocksSubmoduleStore::open_converting(scratch.join(".")).unwrap_err();
        let via_dot_text = format!("{via_dot:#}");
        assert!(
            via_dot_text.contains("already open in this process")
                || via_dot_text.contains("close scratch"),
            "{via_dot_text}"
        );
        assert!(!via_dot_text.contains("LOCK is held"), "{via_dot_text}");
        assert_eq!(dir_names(&scratch)?, before, "{via_dot_text}");

        let stats_before = claim_directory_stats();
        let converting = RocksSubmoduleStore::open_converting(&link);
        let converting_text = match &converting {
            Ok(_) => "ok".to_owned(),
            Err(err) => format!("{err:#}"),
        };
        let after_converting = dir_names(&scratch)?;
        assert_eq!(
            after_converting, before,
            "converting alias reached rocksdb and rewrote the parked directory: {converting_text}"
        );
        assert!(
            converting_text.contains("already open in this process"),
            "{converting_text}"
        );
        assert_one_claim_stat(stats_before);
        assert!(
            !converting_text.contains("Corruption") && !converting_text.contains("No such file"),
            "{converting_text}"
        );

        let writable = RocksSubmoduleStore::open(&link).unwrap_err();
        assert_live_open_refused(&writable);
        assert_eq!(dir_names(&scratch)?, before);

        // The old spelling is marker-blocked. It must not create a new database.
        assert!(!rocks.exists());
        let old = RocksSubmoduleStore::open(&rocks).unwrap_err();
        let old_text = format!("{old:#}");
        assert!(
            old_text.contains("close scratch") || old_text.contains("already open in this process"),
            "{old_text}"
        );
        assert!(!rocks.join("CURRENT").exists(), "{old_text}");
        Ok(())
    }

    /// A path whose last component does not exist yet has no inode. The
    /// claim is that canonical path only. The parent directory stays openable,
    /// and a sibling database opens beside it.
    #[test]
    fn probe_claim_missing_final_component_does_not_claim_the_parent() -> eyre::Result<()> {
        let dir = temp_dir("probe_claim_fresh");
        let parent = dir.path().join("parent");
        fs::create_dir(&parent)?;
        {
            let parent_db = RocksSubmoduleStore::open(&parent)?;
            put_offset(&parent_db, 3, 3)?;
        }
        let parent_link = dir.path().join("parent-link");
        std::os::unix::fs::symlink(&parent, &parent_link)?;
        let via_link = parent_link.join("fresh");
        let via_real = parent.join("fresh");
        let opened = RocksSubmoduleStore::open(&via_link)?;
        put_offset(&opened, 0, 4)?;
        assert_live_open_refused(&RocksSubmoduleStore::open(&via_real).unwrap_err());

        let parent_reader = RocksSubmoduleStore::open_read_only(&parent)?;
        assert!(path_hashes_at(&parent_reader, 3)?.is_some());
        drop(parent_reader);

        let sibling = parent.join("sibling");
        let other = RocksSubmoduleStore::open(&sibling)?;
        put_offset(&other, 1, 5)?;
        assert!(path_hashes_at(&other, 1)?.is_some());
        drop(other);

        drop(opened);
        let reader = RocksSubmoduleStore::open_read_only(&via_real)?;
        assert!(path_hashes_at(&reader, 0)?.is_some());
        drop(reader);
        drop(RocksSubmoduleStore::open(&via_link)?);
        let parent_again = RocksSubmoduleStore::open_read_only(&parent_link)?;
        assert!(path_hashes_at(&parent_again, 3)?.is_some());
        drop(parent_again);
        Ok(())
    }

    /// A marker that is not RocksDB v1 is refused. Open writes nothing.
    #[test]
    fn foreign_marker_is_refused_and_writes_nothing() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_foreign");
        let path = dir.path().join("index");
        fs::create_dir_all(&path)?;
        let marker = "foreign-layout\n";
        fs::write(path.join(SCHEMA_FILE), marker)?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unrecognised RocksDB index layout"), "{text}");
        assert!(text.contains("RocksDB v1 only"), "{text}");
        assert!(!path.join("CURRENT").is_file());
        assert_eq!(fs::read_to_string(path.join(SCHEMA_FILE))?, marker);
        Ok(())
    }

    /// An existing database that lacks a RocksDB v1 family is refused.
    /// Open does not create the family. The offset family is created with
    /// default options. The placement family keeps the content prefix
    /// extractor so open fails on the missing family, not on that extractor.
    #[test]
    fn missing_known_family_is_refused_and_not_created() -> eyre::Result<()> {
        let cases: &[(&str, &str, &[&str], bool)] = &[
            (
                "submodule_rocks_missing_cf",
                CHUNK_META_BY_OFFSET_CF,
                &[
                    "default",
                    "PlacementIntervals",
                    "DataRootPlacements",
                    "PendingBodyMigrations",
                    "Metadata",
                    "ChunkOffsetsByContentHash",
                ],
                false,
            ),
            (
                "submodule_rocks_missing_placement",
                "PlacementIntervals",
                &[
                    "default",
                    "DataRootPlacements",
                    "PendingBodyMigrations",
                    "Metadata",
                    "ChunkOffsetsByContentHash",
                    CHUNK_META_BY_OFFSET_CF,
                ],
                true,
            ),
        ];
        for &(prefix, missing, names, with_prefix) in cases {
            let dir = temp_dir(prefix);
            let path = dir.path().join("index");
            let mut db_opts = Options::default();
            db_opts.create_if_missing(true);
            db_opts.create_missing_column_families(true);
            let families = names.iter().map(|name| {
                let opts = if with_prefix {
                    reopen_options(name)
                } else {
                    Options::default()
                };
                rocksdb::ColumnFamilyDescriptor::new(*name, opts)
            });
            {
                let _db = DB::open_cf_descriptors(&db_opts, &path, families)?;
            }
            fs::write(path.join(SCHEMA_FILE), SUBMODULE_SCHEMA_ROCKS_V1)?;
            let before = DB::list_cf(&Options::default(), &path)?;
            let err = RocksSubmoduleStore::open(&path).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(missing), "{text}");
            assert!(text.contains("does not create it empty"), "{text}");
            let after = DB::list_cf(&Options::default(), &path)?;
            assert_eq!(before, after);
            assert!(!after.iter().any(|name| name == missing));
        }
        Ok(())
    }

    /// Metadata key 6. Three little-endian u64 bitsets: compat, ro_compat, incompat.
    const FEATURE_FLAGS_KEY: [u8; 4] = 6_u32.to_le_bytes();

    fn feature_flag_bytes(compat: u64, ro_compat: u64, incompat: u64) -> [u8; 24] {
        let mut bytes = [0_u8; 24];
        bytes[0..8].copy_from_slice(&compat.to_le_bytes());
        bytes[8..16].copy_from_slice(&ro_compat.to_le_bytes());
        bytes[16..24].copy_from_slice(&incompat.to_le_bytes());
        bytes
    }

    /// Writable reopen has to keep the content family's prefix extractor.
    /// Read-only open accepts default options. A writable open does not.
    fn reopen_options(name: &str) -> Options {
        let mut opts = Options::default();
        if name == "ChunkOffsetsByContentHash" {
            opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(32));
        }
        opts
    }

    fn reopen_for_edit(path: &Path) -> eyre::Result<DB> {
        let names = DB::list_cf(&Options::default(), path)?;
        let mut db_opts = Options::default();
        db_opts.create_if_missing(false);
        db_opts.create_missing_column_families(false);
        let families = names.into_iter().map(|name| {
            let opts = reopen_options(&name);
            rocksdb::ColumnFamilyDescriptor::new(name, opts)
        });
        DB::open_cf_descriptors(&db_opts, path, families).wrap_err("reopen rocksdb for edit")
    }

    fn read_only_value(path: &Path, family: &str, key: &[u8]) -> eyre::Result<Option<Vec<u8>>> {
        let names = DB::list_cf(&Options::default(), path)?;
        let mut db_opts = Options::default();
        db_opts.create_if_missing(false);
        db_opts.create_missing_column_families(false);
        let families = names
            .into_iter()
            .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, Options::default()));
        let db = DB::open_cf_descriptors_read_only(&db_opts, path, families, false)
            .wrap_err("read rocksdb")?;
        let handle = db
            .cf_handle(family)
            .ok_or_else(|| eyre::eyre!("missing column family {family}"))?;
        Ok(db.get_cf(&handle, key)?)
    }

    fn put_metadata_bytes(path: &Path, key: &[u8], value: &[u8]) -> eyre::Result<()> {
        let db = reopen_for_edit(path)?;
        let handle = db
            .cf_handle("Metadata")
            .ok_or_else(|| eyre::eyre!("missing Metadata"))?;
        db.put_cf(&handle, key, value)?;
        Ok(())
    }

    fn delete_metadata_bytes(path: &Path, key: &[u8]) -> eyre::Result<()> {
        let db = reopen_for_edit(path)?;
        let handle = db
            .cf_handle("Metadata")
            .ok_or_else(|| eyre::eyre!("missing Metadata"))?;
        db.delete_cf(&handle, key)?;
        Ok(())
    }

    /// A create, and a later writable open of a database that lost the row,
    /// both store 24 zero bytes. A read-only open does not write the row.
    #[test]
    fn writable_open_stamps_absent_feature_flags_as_zeros() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_feature_stamp");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let zeros = vec![0_u8; 24];
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(zeros.clone())
        );

        delete_metadata_bytes(&path, &FEATURE_FLAGS_KEY)?;
        drop(RocksSubmoduleStore::open_read_only(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            None
        );

        drop(RocksSubmoduleStore::open(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(zeros)
        );
        Ok(())
    }

    /// An unknown family and an unknown compat bit both open. Neither is rewritten.
    #[test]
    fn unknown_compat_family_and_bit_open_and_stay_untouched() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_compat");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);

        let flags = feature_flag_bytes(0b101, 0, 0);
        let family = "FutureIndex";
        let payload = b"leave-me";
        {
            let mut db = reopen_for_edit(&path)?;
            db.create_cf(family, &Options::default())?;
            let meta = db
                .cf_handle("Metadata")
                .ok_or_else(|| eyre::eyre!("missing Metadata"))?;
            db.put_cf(&meta, FEATURE_FLAGS_KEY, flags)?;
            let extra = db
                .cf_handle(family)
                .ok_or_else(|| eyre::eyre!("missing {family}"))?;
            db.put_cf(&extra, b"k", payload)?;
        }

        drop(RocksSubmoduleStore::open(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );
        assert_eq!(
            read_only_value(&path, family, b"k")?,
            Some(payload.to_vec())
        );
        let names = DB::list_cf(&Options::default(), &path)?;
        assert!(names.iter().any(|name| name == family), "{names:?}");
        Ok(())
    }

    /// A writable open lists every family. An unknown family must not compact:
    /// default options would rewrite its SSTs. Known families keep compaction.
    #[test]
    fn unknown_family_opens_with_auto_compaction_disabled() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_unknown_cf_compact");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);

        let family = "FutureIndex";
        {
            let mut db = reopen_for_edit(&path)?;
            db.create_cf(family, &Options::default())?;
            let extra = db
                .cf_handle(family)
                .ok_or_else(|| eyre::eyre!("missing {family}"))?;
            db.put_cf(&extra, b"k", b"leave-me")?;
        }

        drop(RocksSubmoduleStore::open(&path)?);
        let opts = persisted_options(&path)?;
        assert_eq!(
            cf_option(&opts, family, "prefix_extractor").as_deref(),
            Some("nullptr"),
            "a family with no prefix extractor still opens"
        );
        assert_eq!(
            cf_option(&opts, family, "disable_auto_compactions").as_deref(),
            Some("true"),
            "{opts}"
        );
        assert_ne!(
            cf_option(&opts, "Metadata", "disable_auto_compactions").as_deref(),
            Some("true"),
            "known families stay on their own compaction setting"
        );
        assert_eq!(
            read_only_value(&path, family, b"k")?,
            Some(b"leave-me".to_vec())
        );
        Ok(())
    }

    /// A newer family with a prefix extractor is not opened for write.
    /// Recovery would flush its WAL with default table options and drop the
    /// extractor. Read-only open does not flush, so the key stays readable.
    #[test]
    fn unknown_prefix_family_refuses_writable_open_and_keeps_the_key() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_unknown_prefix");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);

        let family = "FuturePrefixed";
        let payload = b"leave-me";
        {
            let mut db = reopen_for_edit(&path)?;
            let mut opts = Options::default();
            opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(8));
            db.create_cf(family, &opts)?;
            let extra = db
                .cf_handle(family)
                .ok_or_else(|| eyre::eyre!("missing {family}"))?;
            db.put_cf(&extra, b"k", payload)?;
        }

        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(family), "{text}");
        assert!(text.contains("prefix extractor"), "{text}");
        assert_eq!(
            read_only_value(&path, family, b"k")?,
            Some(payload.to_vec()),
            "the refused open must not rewrite the family"
        );
        drop(RocksSubmoduleStore::open_read_only(&path)?);
        assert_eq!(
            read_only_value(&path, family, b"k")?,
            Some(payload.to_vec())
        );
        Ok(())
    }

    /// Any incompat bit this binary does not define refuses read and write.
    /// The stored bytes stay.
    #[test]
    fn unknown_incompat_bit_refuses_and_keeps_the_row() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_incompat");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let flags = feature_flag_bytes(0, 0, 1 << 3);
        put_metadata_bytes(&path, &FEATURE_FLAGS_KEY, &flags)?;

        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("too old"), "{text}");
        assert!(text.contains("incompatible feature bits 3 (0x8)"), "{text}");
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );

        let err = RocksSubmoduleStore::open_read_only(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("too old"), "{text}");
        assert!(text.contains("incompatible feature bits 3 (0x8)"), "{text}");
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );
        Ok(())
    }

    /// An unknown ro_compat bit refuses a writable open and still allows read-only.
    #[test]
    fn unknown_ro_compat_bit_refuses_writable_open_only() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_ro_compat");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let flags = feature_flag_bytes(0, 1, 0);
        put_metadata_bytes(&path, &FEATURE_FLAGS_KEY, &flags)?;

        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("too old"), "{text}");
        assert!(
            text.contains("read-only compatible feature bits 0 (0x1)"),
            "{text}"
        );
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );

        drop(RocksSubmoduleStore::open_read_only(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );
        Ok(())
    }

    /// SST, WAL, MANIFEST, and CURRENT bytes. A recovery flush replaces these.
    fn recovery_snapshot(path: &Path) -> eyre::Result<Vec<(String, Vec<u8>)>> {
        let mut rows = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let keep = name == "CURRENT"
                || name.starts_with("MANIFEST-")
                || name.ends_with(".sst")
                || name.ends_with(".log");
            if !keep {
                continue;
            }
            rows.push((name.into_owned(), fs::read(entry.path())?));
        }
        rows.sort();
        Ok(rows)
    }

    fn assert_same_recovery_files(before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)]) {
        let names = |rows: &[(String, Vec<u8>)]| -> Vec<(String, usize)> {
            rows.iter()
                .map(|(name, bytes)| (name.clone(), bytes.len()))
                .collect()
        };
        assert_eq!(names(before), names(after), "recovery files changed");
        for (left, right) in before.iter().zip(after.iter()) {
            assert_eq!(left.1, right.1, "bytes changed in {}", left.0);
        }
    }

    /// The refusal must leave the WAL tail in place. A flush would publish an
    /// L0 SST under this binary's table options and retire that log.
    fn assert_refused_open_keeps_recovery_files(
        path: &Path,
        open_err_contains: &str,
    ) -> eyre::Result<()> {
        let before = recovery_snapshot(path)?;
        let wal_bytes: usize = before
            .iter()
            .filter(|(name, _)| name.ends_with(".log"))
            .map(|(_, bytes)| bytes.len())
            .sum();
        assert!(
            wal_bytes > 0,
            "the refusal fixture must leave a non-empty WAL"
        );
        let err = RocksSubmoduleStore::open(path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(open_err_contains), "{text}");
        assert_same_recovery_files(&before, &recovery_snapshot(path)?);
        Ok(())
    }

    /// An unknown incompat bit sits in the WAL. The refused writable open must
    /// not flush that tail or replace the manifest.
    #[test]
    fn refused_incompat_open_keeps_the_wal() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_incompat_wal");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let flags = feature_flag_bytes(0, 0, 1 << 3);
        put_metadata_bytes(&path, &FEATURE_FLAGS_KEY, &flags)?;
        assert_refused_open_keeps_recovery_files(&path, "incompatible feature bits")?;
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );
        Ok(())
    }

    /// An unknown ro_compat bit refuses a writable open the same way.
    /// Read-only open stays allowed and also leaves the log in place.
    #[test]
    fn refused_ro_compat_open_keeps_the_wal() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_ro_compat_wal");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let flags = feature_flag_bytes(0, 1, 0);
        put_metadata_bytes(&path, &FEATURE_FLAGS_KEY, &flags)?;
        assert_refused_open_keeps_recovery_files(&path, "read-only compatible feature bits")?;
        let before = recovery_snapshot(&path)?;
        drop(RocksSubmoduleStore::open_read_only(&path)?);
        assert_same_recovery_files(&before, &recovery_snapshot(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &FEATURE_FLAGS_KEY)?,
            Some(flags.to_vec())
        );
        Ok(())
    }

    /// A schema row this binary does not read is refused before recovery flush.
    #[test]
    fn refused_schema_row_open_keeps_the_wal() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_schema_row_wal");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let key = 1_u32.to_le_bytes();
        let foreign = b"irys-submodule-index rocksdb 99\n";
        put_metadata_bytes(&path, &key, foreign)?;
        assert_refused_open_keeps_recovery_files(&path, "unrecognised RocksDB index layout")?;
        assert_eq!(
            read_only_value(&path, "Metadata", &key)?,
            Some(foreign.to_vec())
        );
        Ok(())
    }

    /// The check before a writable open must still let a normal reopen apply
    /// a WAL-only row.
    #[test]
    fn successful_reopen_recovers_a_wal_only_metadata_row() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_wal_only_row");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let key = 9_u32.to_le_bytes();
        let value = b"wal-only-row";
        put_metadata_bytes(&path, &key, value)?;
        drop(RocksSubmoduleStore::open(&path)?);
        assert_eq!(
            read_only_value(&path, "Metadata", &key)?,
            Some(value.to_vec())
        );
        Ok(())
    }

    fn assert_incomplete_open_refused(path: &Path) -> eyre::Result<()> {
        let err = RocksSubmoduleStore::open(path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("with --replace-incomplete"), "{text}");
        assert!(text.contains("SCHEMA"), "{text}");
        Ok(())
    }

    /// A non-empty directory with no SCHEMA is not a new database.
    /// `CONVERTING` is an unfinished conversion. A stray file is the same refusal.
    /// Open writes nothing in either case.
    #[test]
    fn incomplete_directory_refuses_and_writes_nothing() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_converting");
        let path = dir.path().join("index");
        fs::create_dir_all(&path)?;
        let sentinel = b"irys-submodule-index convert-incomplete\n";
        fs::write(path.join("CONVERTING"), sentinel)?;
        assert_incomplete_open_refused(&path)?;
        assert_eq!(fs::read(path.join("CONVERTING"))?, sentinel);
        let mut names = Vec::new();
        for entry in fs::read_dir(&path)? {
            names.push(entry?.file_name());
        }
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from("CONVERTING")]);

        let dir = temp_dir("submodule_rocks_stray");
        let path = dir.path().join("index");
        fs::create_dir_all(&path)?;
        fs::write(path.join("notes"), b"stray")?;
        assert_incomplete_open_refused(&path)?;
        assert_eq!(fs::read(path.join("notes"))?, b"stray");
        assert!(!path.join("CURRENT").is_file());
        assert!(!path.join(SCHEMA_FILE).exists());
        Ok(())
    }

    /// `CURRENT` without `SCHEMA` and without `CONVERTING` is a crashed first open.
    /// `--replace-incomplete` does not delete that directory.
    #[test]
    fn crashed_first_open_names_delete_not_replace_incomplete() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_fresh_crash");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        fs::remove_file(path.join(SCHEMA_FILE))?;
        assert!(path.join("CURRENT").is_file());
        assert!(!path.join("CONVERTING").exists());
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("SCHEMA"), "{text}");
        assert!(text.contains("Delete this directory"), "{text}");
        assert!(text.contains("no CONVERTING"), "{text}");
        assert!(!text.contains("with --replace-incomplete"), "{text}");
        assert!(!path.join(SCHEMA_FILE).exists());
        Ok(())
    }

    /// `SCHEMA` already written, with `CONVERTING` still present, is a finished index.
    /// The sentinel is removed after the marker. Open must not refuse it.
    #[test]
    fn schema_beside_converting_still_opens() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_schema_converting");
        let path = dir.path().join("index");
        drop(RocksSubmoduleStore::open(&path)?);
        let sentinel = b"irys-submodule-index convert-incomplete\n";
        fs::write(path.join("CONVERTING"), sentinel)?;
        drop(RocksSubmoduleStore::open(&path)?);
        assert_eq!(fs::read(path.join("CONVERTING"))?, sentinel);
        assert_eq!(
            fs::read_to_string(path.join(SCHEMA_FILE))?,
            SUBMODULE_SCHEMA_ROCKS_V1
        );
        Ok(())
    }

    /// `SCHEMA` with no `CURRENT` is a finished directory that lost its manifest
    /// pointer. Open must not create an empty index over the SST files.
    /// Restoring `CURRENT` returns the flushed row. An empty directory, which
    /// has neither file, still creates.
    #[test]
    fn schema_without_current_refuses_and_keeps_the_flushed_row() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_schema_no_current");
        let path = dir.path().join("index");
        let offset = PartitionChunkOffset::from(4);
        {
            let store = RocksSubmoduleStore::open(&path)?;
            place_span(&store, 0, 7, H256::repeat_byte(4), 0, 1, 0, &[4], None)?;
            store.flush_memtables()?;
            assert!(
                store
                    .view(|tx| tx.get_data_path_by_offset(offset))?
                    .is_some()
            );
        }
        let current = fs::read(path.join("CURRENT"))?;
        let manifest_name = String::from_utf8(current.clone())
            .unwrap()
            .trim()
            .to_owned();
        let manifest = fs::read(path.join(&manifest_name))?;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".log") {
                fs::remove_file(entry.path())?;
            }
        }
        fs::remove_file(path.join("CURRENT"))?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not a new database"), "{text}");
        assert!(text.contains("CURRENT"), "{text}");
        assert!(text.contains("Restore"), "{text}");
        assert!(!path.join("CURRENT").is_file());
        assert_eq!(fs::read(path.join(&manifest_name))?, manifest);
        let err = RocksSubmoduleStore::open_read_only(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not a new database"), "{text}");
        assert_eq!(fs::read(path.join(&manifest_name))?, manifest);

        fs::write(path.join("CURRENT"), &current)?;
        let store = RocksSubmoduleStore::open(&path)?;
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(offset))?
                .is_some(),
            "restoring CURRENT must return the flushed row"
        );
        Ok(())
    }

    /// A RocksDB v1 proof outside every interval is not stored. A covered
    /// offset in the same batch still is. The skipped offset is the result.
    #[test]
    fn uncovered_data_path_is_reported_and_not_stored() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_uncovered_path");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let covered = PartitionChunkOffset::from(1);
        let missing = PartitionChunkOffset::from(4);
        let root = H256::repeat_byte(9);
        store.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(covered, covered, Some(root))?;
            tx.add_data_root_info(
                root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: 32,
                },
            )
        })?;
        let missed = store.update(|tx| {
            tx.write_data_path_updates(vec![
                (missing, H256::repeat_byte(4), vec![4, 4]),
                (covered, H256::repeat_byte(1), vec![1, 1]),
            ])
        })?;
        assert_eq!(missed, vec![missing]);
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(missing))?
                .is_none()
        );
        assert_eq!(
            store.view(|tx| tx.get_data_path_by_offset(covered))?,
            Some(vec![1, 1])
        );
        store.update(|tx| tx.add_full_data_path(missing, vec![9, 9]))?;
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(missing))?
                .is_none()
        );
        Ok(())
    }

    /// `clear` is one range tombstone per non-empty index family. It does not
    /// read values. 100k rows plus one large value must finish well under a scan.
    #[test]
    fn clear_of_a_large_index_leaves_only_metadata() -> eyre::Result<()> {
        const ROWS: u32 = 100_000;
        let dir = temp_dir("submodule_rocks_clear_large");
        let path = dir.path().join("index");
        let store = RocksSubmoduleStore::open(&path)?;
        let tx_hash = H256::repeat_byte(9);
        let data_root = H256::repeat_byte(4);
        store.update(|tx| {
            tx.add_tx_leaf_binding(
                tx_hash,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::repeat_byte(1),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(ROWS - 1),
                Some(tx_hash),
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 64,
                },
            )?;
            tx.add_pending_body_migration(
                PartitionChunkOffset::from(3),
                &PendingBodyMigration {
                    data_root,
                    data_size: 64,
                    start_offset: RelativeChunkOffset(0),
                    block_height: 1,
                    attempts: 0,
                },
            )?;
            Ok(())
        })?;
        for start in (0..ROWS).step_by(20_000) {
            let end = start.saturating_add(20_000).min(ROWS);
            store.update(|tx| {
                let updates = (start..end)
                    .map(|offset| {
                        (
                            PartitionChunkOffset::from(offset),
                            H256::repeat_byte((offset % 200) as u8),
                            vec![offset as u8],
                        )
                    })
                    .collect();
                tx.write_data_path_updates(updates)
            })?;
        }
        store.update(|tx| {
            tx.add_full_tx_path(tx_hash, vec![9_u8; 64 * 1024])?;
            tx.add_full_data_path(PartitionChunkOffset::from(0), vec![7_u8; 256 * 1024])?;
            Ok(())
        })?;
        assert!(store.cf_key_count(Cf::OffsetMeta)? >= usize::try_from(ROWS)?);

        let started = std::time::Instant::now();
        store.update(|tx| tx.clear())?;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "clear took {elapsed:?}"
        );

        for cf in families() {
            if matches!(cf, Cf::Metadata) {
                continue;
            }
            assert_eq!(store.cf_key_count(cf)?, 0, "{}", cf.name());
        }
        assert_eq!(store.cf_key_count(Cf::Metadata)?, 2);
        let payload = store.schema_bytes()?.expect("schema row");
        assert_eq!(payload, SUBMODULE_SCHEMA_ROCKS_V1.as_bytes());
        assert!(
            store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(50_000)))?
                .is_none()
        );
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(0)))?
                .is_none()
        );
        assert!(store.view(|tx| tx.get_full_tx_path(tx_hash))?.is_none());
        assert!(
            store
                .view(|tx| tx.get_data_root_infos_for_data_root(data_root))?
                .is_none()
        );
        assert!(
            store
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(3)))?
                .is_none()
        );
        assert_eq!(store.interval_row_count()?, 0);
        drop(store);

        let store = RocksSubmoduleStore::open(&path)?;
        assert_eq!(store.cf_key_count(Cf::OffsetMeta)?, 0);
        assert_eq!(store.cf_key_count(Cf::Metadata)?, 2);
        assert_eq!(
            store.schema_bytes()?.expect("schema row"),
            SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()
        );
        Ok(())
    }

    #[test]
    fn content_hash_index_tracks_writes_rewrites_and_deletes() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_content_index");
        let path = dir.path().join("index");
        let store = RocksSubmoduleStore::open(&path)?;
        // A data path needs a covering interval. Offsets 4 and 9 sit inside it.
        place_span(&store, 0, 20, H256::repeat_byte(9), 0, 1, 0, &[], None)?;
        let hash_a = H256::repeat_byte(1);
        let hash_b = H256::repeat_byte(2);
        let hash_c = H256::repeat_byte(3);
        let at4 = PartitionChunkOffset::from(4);
        let at9 = PartitionChunkOffset::from(9);
        store.update(|tx| {
            tx.add_full_data_path(at4, content_leaf(1))?;
            let seen = tx.offsets_for_content_hash(hash_a)?;
            assert_eq!(seen, vec![at4]);
            tx.add_full_data_path(at4, content_leaf(3))?;
            tx.add_full_data_path(at4, content_leaf(2))?;
            assert!(tx.offsets_for_content_hash(hash_a)?.is_empty());
            assert!(tx.offsets_for_content_hash(hash_c)?.is_empty());
            assert_eq!(tx.offsets_for_content_hash(hash_b)?, vec![at4]);
            Ok(())
        })?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_b))?,
            vec![at4]
        );
        assert!(
            store
                .view(|tx| tx.offsets_for_content_hash(hash_a))?
                .is_empty()
        );
        store.update(|tx| tx.add_full_data_path(at9, content_leaf(1)))?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_a))?,
            vec![at9]
        );
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_b))?,
            vec![at4]
        );
        store.update(|tx| tx.add_full_data_path(at4, content_leaf(1)))?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_a))?,
            vec![at4, at9]
        );
        assert!(
            store
                .view(|tx| tx.offsets_for_content_hash(hash_b))?
                .is_empty()
        );
        drop(store);
        let store = RocksSubmoduleStore::open(&path)?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_a))?,
            vec![at4, at9]
        );
        assert_content_index(&store)?;
        store.update(|tx| tx.clear_paths_in_inclusive_range(at9, at9))?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(hash_a))?,
            vec![at4]
        );
        store.drop_presence_in_ranges(&[(at4, at4)])?;
        assert!(
            store
                .view(|tx| tx.offsets_for_content_hash(hash_a))?
                .is_empty()
        );
        store.update(|tx| {
            tx.add_full_data_path(at4, content_leaf(1))?;
            tx.add_full_data_path(at9, vec![7_u8])?;
            Ok(())
        })?;
        assert_content_index(&store)?;
        assert_eq!(store.cf_key_count(Cf::Content)?, 1);
        store.update(|tx| tx.clear())?;
        assert_eq!(store.cf_key_count(Cf::Content)?, 0);
        assert_content_index(&store)?;
        Ok(())
    }

    /// A rollback clear must not copy every offset value in the span.
    ///
    /// Checksum rows have no content key. Proof rows still lose `hash‖offset`.
    /// A neighbor outside the span stays. A failed synced write leaves both
    /// families as they were: the content deletes share that commit.
    #[test]
    fn clear_streams_offset_values_and_keeps_the_commit_atomic() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_clear_stream");
        // Three proofs and a batch of two: the third key is deleted only if
        // the scan resumes after it drops the first batch.
        let _batch = set_clear_content_key_batch(2);
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let kept = 50_u32;
        place_span(
            &store,
            0,
            kept,
            H256::repeat_byte(4),
            0,
            1,
            0,
            &[1, 20, 39, kept],
            None,
        )?;
        let checksums: Vec<u32> = (0..=39)
            .filter(|offset| !matches!(offset, 1 | 20 | 39))
            .collect();
        store.update(|tx| {
            let rows = checksums
                .iter()
                .copied()
                .map(|offset| PackedOffsetRow {
                    offset: PartitionChunkOffset::from(offset),
                    crc: Some(offset),
                    proof: None,
                })
                .collect();
            tx.write_packed_offset_rows(rows)
        })?;
        let span_rows = checksums.len() + 3;
        assert!(span_rows > 1);

        reset_clear_offset_value_peak();
        store.fail_next_synced_write();
        let err = store
            .update(|tx| {
                tx.clear_paths_in_inclusive_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(39),
                )
            })
            .unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(1)))?,
            vec![PartitionChunkOffset::from(1)]
        );
        assert!(
            store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(2)))?
                .is_some(),
            "a rejected clear leaves the checksum row"
        );
        assert!(store.wal_sync_failed());
        drop(store);

        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        reset_clear_offset_value_peak();
        reset_clear_content_scan_passes();
        store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(39),
            )
        })?;
        let peak = clear_offset_value_peak();
        assert_eq!(
            peak, 1,
            "clear held {peak} offset values, span has {span_rows}"
        );
        assert_eq!(
            clear_content_scan_passes(),
            2,
            "batch of 2 must resume to delete the third proof"
        );
        for offset in [1_u32, 20, 39] {
            assert!(
                store
                    .view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(offset as u8)))?
                    .is_empty(),
                "offset {offset} content key remains"
            );
            assert!(
                store
                    .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(offset)))?
                    .is_none(),
                "offset {offset} proof remains"
            );
        }
        for offset in &checksums {
            assert!(
                store
                    .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(*offset)))?
                    .is_none(),
                "checksum {offset} remains"
            );
        }
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(kept as u8)))?,
            vec![PartitionChunkOffset::from(kept)]
        );
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(kept)))?
                .is_some()
        );
        assert_content_index(&store)?;
        Ok(())
    }

    /// A resume that seeks the next 4-byte offset skips a longer key that sorts
    /// between those encodings. `DeleteRange` would then remove it. The clear
    /// must fail and leave the span. A later clear that starts after the key
    /// still drops the proofs and checksums in its own span.
    #[test]
    fn clear_resume_keeps_a_key_that_sorts_between_offsets() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_clear_resume_gap");
        let _batch = set_clear_content_key_batch(1);
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let kept = 50_u32;
        place_span(
            &store,
            0,
            kept,
            H256::repeat_byte(4),
            0,
            1,
            0,
            &[1, 10, 14, kept],
            None,
        )?;
        let checksums = [3_u32, 8, 12];
        store.update(|tx| {
            let rows = checksums
                .into_iter()
                .map(|offset| PackedOffsetRow {
                    offset: PartitionChunkOffset::from(offset),
                    crc: Some(offset),
                    proof: None,
                })
                .collect();
            tx.write_packed_offset_rows(rows)
        })?;
        // Sorts after offset 1 and before offset 2. Batch size 1 fills on the
        // proof at 1, so the next seek is the one that can skip this key.
        let gap_key = [0_u8, 0, 0, 1, 0];
        let handle = cf_handle(&store.db, Cf::OffsetMeta)?;
        store.db.put_cf(&handle, gap_key, [])?;
        store.db.flush_cf(&handle)?;

        let wide = store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        let gap_still = store.db.get_cf(&handle, gap_key)?.is_some();
        let content_10 = store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(10)))?;
        let path_1 = store
            .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(1)))?
            .is_some();
        let path_10 = store
            .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(10)))?
            .is_some();
        let Err(wide_err) = wide else {
            panic!(
                "clear succeeded; gap_still={gap_still} content_10={content_10:?} path_1={path_1} path_10={path_10}"
            );
        };
        assert!(wide_err.to_string().contains("not an offset"), "{wide_err}");
        assert!(
            gap_still && path_1 && path_10 && content_10 == vec![PartitionChunkOffset::from(10)],
            "gap_still={gap_still} content_10={content_10:?} path_1={path_1} path_10={path_10}"
        );
        assert!(
            store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(3)))?
                .is_some(),
            "a rejected clear leaves the checksum row"
        );
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(1)))?,
            vec![PartitionChunkOffset::from(1)]
        );

        store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(6),
                PartitionChunkOffset::from(20),
            )
        })?;
        assert!(
            store.db.get_cf(&handle, gap_key)?.is_some(),
            "the gap key is outside the cleared span"
        );
        for offset in [10_u32, 14] {
            assert!(
                store
                    .view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(offset as u8)))?
                    .is_empty(),
                "offset {offset} content key remains"
            );
            assert!(
                store
                    .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(offset)))?
                    .is_none(),
                "offset {offset} proof remains"
            );
        }
        for offset in [8_u32, 12] {
            assert!(
                store
                    .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                    .is_none(),
                "checksum {offset} remains"
            );
        }
        assert!(
            store
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(3)))?
                .is_some()
        );
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(1)))?,
            vec![PartitionChunkOffset::from(1)]
        );
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(kept as u8)))?,
            vec![PartitionChunkOffset::from(kept)]
        );
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(1)))?
                .is_some()
        );
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(10)))?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn malformed_interval_key_fails_the_placement_scans() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_malformed_interval_key");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        place_span(&store, 0, 7, H256::repeat_byte(4), 0, 1, 0, &[], None)?;
        assert_eq!(store.read_placements(None, 10)?.len(), 1);
        let handle = cf_handle(&store.db, Cf::Interval)?;
        store.db.put_cf(&handle, [0_u8, 0, 0, 9, 0], [])?;
        store.db.flush_cf(&handle)?;

        let scan_err = store.read_placements(None, 10).unwrap_err();
        assert!(scan_err.to_string().contains("not an offset"), "{scan_err}");
        let end_err = store.highest_placement_end().unwrap_err();
        assert!(end_err.to_string().contains("not an offset"), "{end_err}");
        Ok(())
    }

    /// A key that is not a 4-byte offset is inside the span, not its end.
    ///
    /// The clear must fail before `DeleteRange` removes later offset rows
    /// whose content keys were not visited. A span that ends before that key,
    /// or starts after it, still clears.
    #[test]
    fn short_offset_key_fails_the_clear_and_the_span_reads() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_short_offset_key");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        place_span(&store, 0, 20, H256::repeat_byte(4), 0, 1, 0, &[2, 10], None)?;
        let checksum = PartitionChunkOffset::from(12_u32);
        store.update(|tx| {
            tx.write_packed_offset_rows(vec![PackedOffsetRow {
                offset: checksum,
                crc: Some(12),
                proof: None,
            }])
        })?;
        // Sorts after offset 5 and before offset 6, inside `[0, 20]`.
        let short_key = [0_u8, 0, 0, 5, 0];
        let handle = cf_handle(&store.db, Cf::OffsetMeta)?;
        store.db.put_cf(&handle, short_key, [])?;
        store.db.flush_cf(&handle)?;

        let clear_wide = store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        let content_10 = store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(10)))?;
        let path_10 = store
            .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(10)))?
            .is_some();
        let placed_10 = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(10)))?
            .is_some();
        let checksum_kept = store.view(|tx| tx.get_offset_meta(checksum))?.is_some();
        let Err(clear_err) = clear_wide else {
            panic!(
                "clear={clear_wide:?} content {content_10:?} path {path_10} placed {placed_10} checksum {checksum_kept}"
            );
        };
        assert!(
            clear_err.to_string().contains("not an offset"),
            "{clear_err}"
        );
        assert!(
            path_10
                && placed_10
                && checksum_kept
                && content_10 == vec![PartitionChunkOffset::from(10)],
            "content {content_10:?} path {path_10} placed {placed_10} checksum {checksum_kept}"
        );
        let content_2 = store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(2)))?;
        assert_eq!(content_2, vec![PartitionChunkOffset::from(2)]);

        let hashes_wide = store.view(|tx| {
            tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        assert!(
            hashes_wide
                .as_ref()
                .is_err_and(|err| err.to_string().contains("not an offset")),
            "{hashes_wide:?}"
        );
        let metas_wide = store.view(|tx| {
            tx.offset_metas_in_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        assert!(
            metas_wide
                .as_ref()
                .is_err_and(|err| err.to_string().contains("not an offset")),
            "{metas_wide:?}"
        );

        let before = store.view(|tx| {
            tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(4),
            )
        })?;
        assert_eq!(
            before
                .iter()
                .map(|(offset, _)| offset.0)
                .collect::<Vec<_>>(),
            vec![2]
        );
        let after_key = store.view(|tx| {
            tx.offset_metas_in_range(
                PartitionChunkOffset::from(6),
                PartitionChunkOffset::from(20),
            )
        })?;
        assert_eq!(
            after_key
                .iter()
                .map(|(offset, _)| offset.0)
                .collect::<Vec<_>>(),
            vec![10, 12]
        );

        store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(4),
            )
        })?;
        assert!(
            store
                .view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(2)))?
                .is_empty()
        );
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(10)))?,
            vec![PartitionChunkOffset::from(10)]
        );
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(2)))?
                .is_none()
        );
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(10)))?
                .is_some()
        );
        let still_hashes = store.view(|tx| {
            tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        assert!(still_hashes.is_err(), "{still_hashes:?}");

        store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(6),
                PartitionChunkOffset::from(20),
            )
        })?;
        assert!(
            store
                .view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(10)))?
                .is_empty()
        );
        assert!(store.view(|tx| tx.get_offset_meta(checksum))?.is_none());
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(10)))?
                .is_none()
        );
        let still_short = store.view(|tx| {
            tx.offset_metas_in_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )
        });
        assert!(
            still_short.is_err(),
            "the short key stays after clears that do not cover it: {still_short:?}"
        );
        Ok(())
    }

    fn put_checksums(store: &RocksSubmoduleStore, offsets: &[u32]) -> eyre::Result<()> {
        store.update(|tx| {
            let rows = offsets
                .iter()
                .copied()
                .map(|offset| PackedOffsetRow {
                    offset: PartitionChunkOffset::from(offset),
                    crc: Some(offset),
                    proof: None,
                })
                .collect();
            tx.write_packed_offset_rows(rows)?;
            Ok(())
        })
    }

    fn put_raw_offset_key(store: &RocksSubmoduleStore, key: &[u8]) -> eyre::Result<()> {
        let handle = cf_handle(&store.db, Cf::OffsetMeta)?;
        store.db.put_cf(&handle, key, [])?;
        store.db.flush_cf(&handle)?;
        Ok(())
    }

    /// A non-offset key at or past the exclusive end ends the checksum scan.
    /// The same shape of key inside the span still fails it. A hole is still
    /// missing when a key past the span follows the rows.
    #[test]
    fn offset_crc_missing_stops_at_a_key_past_the_span() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_crc_past_span");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        put_checksums(&store, &[0, 1, 2, 3, 4])?;
        let complete = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(0), PartitionChunkOffset::from(4))
        })?;
        assert!(
            !complete,
            "a span with a checksum at every offset is complete"
        );

        // encode(5) is the exclusive end of [0, 4]. This key sorts after it.
        put_raw_offset_key(&store, &[0, 0, 0, 5, 0])?;
        let past = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(0), PartitionChunkOffset::from(4))
        });
        let Ok(false) = past else {
            panic!("span is complete, got {past:?}");
        };
        let tail = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(3), PartitionChunkOffset::from(4))
        })?;
        assert!(!tail, "the tail is complete; the key past it is not a hole");

        let holed = temp_dir("submodule_rocks_crc_hole_then_past");
        let store = RocksSubmoduleStore::open(holed.path().join("index"))?;
        put_checksums(&store, &[0, 1, 3, 4])?;
        put_raw_offset_key(&store, &[0, 0, 0, 5, 0])?;
        let missing = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(0), PartitionChunkOffset::from(4))
        })?;
        assert!(missing, "offset 2 has no checksum");
        let tail = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(3), PartitionChunkOffset::from(4))
        })?;
        assert!(!tail, "offsets 3 and 4 are present");

        let inside = temp_dir("submodule_rocks_crc_inside_span");
        let store = RocksSubmoduleStore::open(inside.path().join("index"))?;
        put_checksums(&store, &[0, 1, 2, 3, 4])?;
        // After offset 4 and before encode(5), so it is still inside [0, 4].
        put_raw_offset_key(&store, &[0, 0, 0, 4, 0])?;
        let scanned = store.view(|tx| {
            tx.offset_crc_missing(PartitionChunkOffset::from(0), PartitionChunkOffset::from(4))
        });
        assert!(
            scanned
                .as_ref()
                .is_err_and(|err| err.to_string().contains("not an offset")),
            "{scanned:?}"
        );
        Ok(())
    }

    #[test]
    fn checksum_only_row_is_not_a_stored_proof() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_crc_only");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let entropy = PartitionChunkOffset::from(2);
        let data = PartitionChunkOffset::from(4);
        place_span(&store, 0, 7, H256::repeat_byte(3), 0, 1, 0, &[4], None)?;
        assert_eq!(store.cf_key_count(Cf::Content)?, 1);
        store.update(|tx| {
            tx.write_packed_offset_rows(vec![
                PackedOffsetRow {
                    offset: entropy,
                    crc: Some(0x1111),
                    proof: None,
                },
                PackedOffsetRow {
                    offset: data,
                    crc: Some(0x2222),
                    proof: None,
                },
            ])
        })?;
        assert_eq!(
            store.cf_key_count(Cf::Content)?,
            0,
            "replacing a proof with a checksum drops the content key"
        );
        assert!(store.view(|tx| tx.get_data_path_by_offset(data))?.is_none());
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(entropy))?
                .is_none()
        );
        let meta = store
            .view(|tx| tx.get_offset_meta(entropy))?
            .expect("checksum row");
        assert_eq!(meta.crc, Some(0x1111));
        assert!(meta.proof.is_none());
        let hashes = store.view(|tx| tx.path_hashes_in_inclusive_range(entropy, entropy))?;
        assert!(
            hashes.iter().all(|(offset, _)| *offset != entropy),
            "a checksum-only row is not a data-path range entry: {hashes:?}"
        );
        let findings = store.audit_index()?;
        assert!(
            findings.iter().all(|line| !line.contains("data path")),
            "{findings:?}"
        );

        store.update(|tx| tx.add_full_data_path(data, content_leaf(4)))?;
        let kept = store
            .view(|tx| tx.get_offset_meta(data))?
            .expect("proof keeps the checksum");
        assert_eq!(kept.crc, Some(0x2222));
        assert!(kept.proof.is_some());
        let removed = store.retain_entropy_crc_rows(&[(data, data)])?;
        assert_eq!(removed.data_path_rows, 1);
        assert!(
            store.view(|tx| tx.get_offset_meta(data))?.is_none(),
            "an entropy span drops a proof row, checksum included"
        );
        assert!(store.view(|tx| tx.get_offset_meta(entropy))?.is_some());
        Ok(())
    }

    fn assert_no_orphans(store: &RocksSubmoduleStore) -> eyre::Result<()> {
        let orphans = store.audit_index()?;
        assert!(orphans.is_empty(), "{orphans:?}");
        assert_content_index(store)
    }

    fn assert_content_index(store: &RocksSubmoduleStore) -> eyre::Result<()> {
        let audit = store.audit_content_index()?;
        assert_eq!(audit.missing_reverse, 0, "{audit:?}");
        assert_eq!(audit.dangling_reverse, 0, "{audit:?}");
        Ok(())
    }

    /// 64-byte data-proof leaf. The first 32 bytes are the content hash.
    fn content_leaf(byte: u8) -> Vec<u8> {
        let mut leaf = vec![0_u8; 64];
        leaf[..32].fill(byte);
        leaf
    }

    fn place_span(
        store: &RocksSubmoduleStore,
        start: u32,
        end: u32,
        data_root: H256,
        ledger: u32,
        block_height: u64,
        tx_index: u32,
        data_at: &[u32],
        pending_at: Option<u32>,
    ) -> eyre::Result<()> {
        let path_hash = H256::repeat_byte(data_root.0[0]);
        store.update(|tx| {
            tx.note_placement_origin(ledger, block_height, tx_index)?;
            tx.add_tx_leaf_binding(
                path_hash,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::repeat_byte(1),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(start),
                PartitionChunkOffset::from(end),
                Some(path_hash),
            )?;
            let data_size = u64::from(end - start + 1) * 32;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(i32::try_from(start).expect("start")),
                    data_size,
                },
            )?;
            for offset in data_at {
                tx.add_full_data_path(
                    PartitionChunkOffset::from(*offset),
                    content_leaf(*offset as u8),
                )?;
            }
            if let Some(offset) = pending_at {
                tx.add_pending_body_migration(
                    PartitionChunkOffset::from(offset),
                    &PendingBodyMigration {
                        data_root,
                        data_size,
                        start_offset: RelativeChunkOffset(i32::try_from(start).expect("start")),
                        block_height,
                        attempts: 0,
                    },
                )?;
            }
            Ok(())
        })
    }

    /// A full scan of `ChunkOffsetsByContentHash` has to cross prefixes.
    ///
    /// The family uses a 32-byte prefix extractor. An iterator that leaves
    /// `total_order_seek` off is not required to return the next hash.
    /// `audit_index` and `Rows::cursor` both walk every key.
    #[test]
    fn content_prefix_scans_see_every_hash() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_content_order");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        place_span(&store, 0, 7, H256::repeat_byte(4), 0, 1, 0, &[], None)?;
        let inside = content_index_key(H256::repeat_byte(1), 1);
        let outside = content_index_key(H256::repeat_byte(0xFE), 100);
        let handle = cf_handle(&store.db, Cf::Content)?;
        store.db.put_cf(&handle, inside, [])?;
        store.db.put_cf(&handle, outside, [])?;
        store.db.flush_cf(&handle)?;

        let findings = store.audit_index()?;
        assert!(
            findings
                .iter()
                .any(|line| { line.contains("content-hash row at 100 is outside every interval") }),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .all(|line| { !line.contains("content-hash row at 1 is outside every interval") }),
            "{findings:?}"
        );

        let snap = store.db.snapshot();
        let rows = Rows {
            db: store.db.as_ref(),
            snap: &snap,
            overlay: None,
            layout: store.layout,
        };
        let mut cursor = rows.cursor(Cf::Content, None)?;
        let mut seen = Vec::new();
        while let Some((key, value)) = cursor.next_kv()? {
            assert!(value.is_empty(), "content value is empty");
            seen.push(key);
        }
        assert_eq!(seen, vec![inside.to_vec(), outside.to_vec()], "{seen:?}");
        Ok(())
    }

    fn content_index_key(hash: H256, offset: u32) -> [u8; 36] {
        let mut key = [0_u8; 36];
        key[..32].copy_from_slice(&hash.0);
        key[32..].copy_from_slice(&offset.to_be_bytes());
        key
    }

    /// Overwrite, uninitialized rollback, expiry, and reset leave no row
    /// outside a live interval.
    #[test]
    fn rows_stay_inside_live_intervals() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_no_leak");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let root_a = H256::repeat_byte(4);
        let root_b = H256::repeat_byte(5);
        place_span(&store, 0, 19, root_a, 1, 42, 3, &[0, 8, 19], Some(2))?;
        assert_no_orphans(&store)?;
        assert_eq!(
            store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(8)))?,
            vec![PartitionChunkOffset::from(8)]
        );
        let at_zero = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(0)))?
            .expect("placement");
        assert_eq!(at_zero.data_root, root_a);
        assert_eq!(at_zero.ledger, Some(1));
        assert_eq!(at_zero.block_height, Some(42));
        assert_eq!(at_zero.tx_index, Some(3));
        assert_eq!(at_zero.end, PartitionChunkOffset::from(19));

        place_span(&store, 8, 11, root_b, 0, 7, 1, &[8], None)?;
        assert_no_orphans(&store)?;
        let left = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(0)))?
            .expect("left remnant");
        assert_eq!(left.data_root, root_a);
        assert_eq!(left.end, PartitionChunkOffset::from(7));
        assert_eq!(left.block_height, Some(42));
        let middle = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(9)))?
            .expect("overwrite");
        assert_eq!(middle.data_root, root_b);
        assert_eq!(middle.ledger, Some(0));
        assert_eq!(middle.tx_index, Some(1));
        assert_eq!(middle.start, PartitionChunkOffset::from(8));
        assert_eq!(middle.end, PartitionChunkOffset::from(11));
        let right = store
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(15)))?
            .expect("right remnant");
        assert_eq!(right.data_root, root_a);
        assert_eq!(right.start, PartitionChunkOffset::from(12));
        assert_eq!(right.tx_index, Some(3));

        let hole = [(PartitionChunkOffset::from(4), PartitionChunkOffset::from(6))];
        store.drop_presence_in_ranges(&hole)?;
        store.drop_uninitialized_coverage(&hole, 32)?;
        assert_no_orphans(&store)?;
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(0)))?
                .is_some()
        );
        assert!(
            store
                .view(|tx| tx.placement_at(PartitionChunkOffset::from(5)))?
                .is_none()
        );
        assert!(
            store
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(2)))?
                .is_some()
        );
        let for_b = store.view(|tx| tx.placements_for_data_root(root_b))?;
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].start, PartitionChunkOffset::from(8));

        let expired = [(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(19),
        )];
        store.drop_presence_in_ranges(&expired)?;
        store.drop_uninitialized_coverage(&expired, 32)?;
        assert_no_orphans(&store)?;
        for cf in [
            Cf::Interval,
            Cf::OffsetMeta,
            Cf::DataRoots,
            Cf::Pending,
            Cf::Content,
        ] {
            assert_eq!(store.cf_key_count(cf)?, 0, "{}", cf.name());
        }

        place_span(&store, 1, 2, root_a, 1, 9, 0, &[1], Some(1))?;
        assert_no_orphans(&store)?;
        store.update(|tx| tx.clear())?;
        assert_no_orphans(&store)?;
        for cf in [
            Cf::Interval,
            Cf::OffsetMeta,
            Cf::DataRoots,
            Cf::Pending,
            Cf::Content,
        ] {
            assert_eq!(store.cf_key_count(cf)?, 0, "{}", cf.name());
        }
        assert_eq!(store.cf_key_count(Cf::Metadata)?, 2);
        Ok(())
    }

    #[test]
    fn refuses_an_unbound_interval_and_an_unowned_info() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_owned");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let root = H256::repeat_byte(4);
        let hash = H256::repeat_byte(6);
        let info = DataRootInfo {
            start_offset: RelativeChunkOffset(0),
            data_size: 32,
        };
        let err = store
            .update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(1),
                    Some(hash),
                )
            })
            .unwrap_err();
        assert!(err.to_string().contains("leaf binding"), "{err}");
        let err = store
            .update(|tx| tx.add_data_root_info(root, &info))
            .unwrap_err();
        assert!(err.to_string().contains("no unfilled placement"), "{err}");
        store.update(|tx| {
            tx.add_tx_leaf_binding(
                hash,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::repeat_byte(1),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(1),
                Some(hash),
            )?;
            let zero = DataRootInfo {
                start_offset: RelativeChunkOffset(0),
                data_size: 0,
            };
            let err = tx.add_data_root_info(root, &zero).unwrap_err();
            assert!(err.to_string().contains("data_size of 0"), "{err}");
            tx.add_data_root_info(root, &info)?;
            Ok(())
        })?;
        store.update(|tx| tx.add_full_data_path(PartitionChunkOffset::from(5), content_leaf(5)))?;
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(5)))?
                .is_none()
        );
        store.update(|tx| {
            tx.add_full_data_path(PartitionChunkOffset::from(0), content_leaf(1))?;
            tx.add_pending_body_migration(
                PartitionChunkOffset::from(0),
                &PendingBodyMigration {
                    data_root: root,
                    data_size: 32,
                    start_offset: RelativeChunkOffset(0),
                    block_height: 1,
                    attempts: 0,
                },
            )?;
            Ok(())
        })?;
        assert_no_orphans(&store)?;
        store.update(|tx| {
            tx.clear_paths_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(1),
            )
        })?;
        assert!(
            store
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(0)))?
                .is_some(),
            "path clear leaves the pending key for the caller"
        );
        store.update(|tx| {
            assert!(tx.del_pending_body_migration(PartitionChunkOffset::from(0))?);
            Ok(())
        })?;
        assert_no_orphans(&store)?;
        Ok(())
    }

    #[test]
    fn clear_in_a_batch_does_not_count_snapshot_keys() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_cleared_count");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        store.update(|tx| {
            tx.add_pending_body_migration(
                PartitionChunkOffset::from(3),
                &PendingBodyMigration {
                    data_root: H256::repeat_byte(1),
                    data_size: 32,
                    start_offset: RelativeChunkOffset(0),
                    block_height: 1,
                    attempts: 0,
                },
            )
        })?;
        store.update(|tx| {
            tx.clear()?;
            let removed = tx.del_pending_body_migrations_in_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(10),
            )?;
            assert_eq!(removed, 0);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn range_delete_hides_pending_from_a_later_check_in_the_same_batch() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_range_hides");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        store.update(|tx| {
            tx.add_pending_body_migration(
                PartitionChunkOffset::from(3),
                &PendingBodyMigration {
                    data_root: H256::repeat_byte(1),
                    data_size: 32,
                    start_offset: RelativeChunkOffset(0),
                    block_height: 1,
                    attempts: 0,
                },
            )
        })?;
        store.update(|tx| {
            let span = (
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(10),
            );
            assert_eq!(tx.del_pending_body_migrations_in_range(span.0, span.1)?, 1);
            assert!(!tx.has_pending_in_inclusive_range(span.0, span.1)?);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn packing_identity_binds_once_and_clear_releases_the_partition() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_packing");
        let path = dir.path().join("index");
        let partition = H256::repeat_byte(1);
        let other = H256::repeat_byte(2);
        let packing = PackingRecord {
            packing_address: [9_u8; 20],
            iterations: 7,
            chunk_size: 32,
            chain_id: 1,
        };
        let other_packing = PackingRecord {
            iterations: 9,
            ..packing
        };
        {
            let store = RocksSubmoduleStore::open(&path)?;
            store.bind_packing_identity(None, &packing)?;
            store.bind_packing_identity(Some(partition), &packing)?;
            store.bind_packing_identity(Some(partition), &packing)?;
            let stored = store
                .read_raw_metadata(MetadataKey::PackingRecord)?
                .expect("packing record");
            assert_eq!(stored.as_slice(), packing.encode().as_slice());
            let err = store
                .bind_packing_identity(Some(other), &packing)
                .unwrap_err();
            assert!(err.to_string().contains("does not match"), "{err}");
            let err = store
                .bind_packing_identity(Some(partition), &other_packing)
                .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("does not match"), "{text}");
            assert!(text.contains("recorded:"), "{text}");
            assert!(text.contains("expected:"), "{text}");
            assert!(text.contains("iterations=7"), "{text}");
            assert!(text.contains("iterations=9"), "{text}");
            assert!(text.contains("submodule-index-convert"), "{text}");
        }
        {
            let store = RocksSubmoduleStore::open(&path)?;
            let err = store
                .bind_packing_identity(Some(other), &packing)
                .unwrap_err();
            assert!(err.to_string().contains("does not match"), "{err}");
            store.update(|tx| tx.clear())?;
            store.bind_packing_identity(Some(other), &packing)?;
            let err = store
                .bind_packing_identity(Some(other), &other_packing)
                .unwrap_err();
            assert!(err.to_string().contains("does not match"), "{err}");
        }
        Ok(())
    }

    #[test]
    fn metadata_flags_are_written_raw_and_read_as_set() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_raw_flags");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        assert!(!store.reset_in_progress()?);
        assert!(!store.chunk_crc_complete()?);
        store.set_reset_in_progress()?;
        store.set_chunk_crc_complete(true)?;
        assert_eq!(
            store
                .read_raw_metadata(MetadataKey::SubmoduleResetInProgress)?
                .as_deref(),
            Some(&[1_u8][..])
        );
        assert_eq!(
            store
                .read_raw_metadata(MetadataKey::ChunkCrcComplete)?
                .as_deref(),
            Some(&[1_u8][..])
        );
        assert!(store.reset_in_progress()?);
        assert!(store.chunk_crc_complete()?);
        Ok(())
    }

    #[test]
    fn presets_change_one_knob_and_open() -> eyre::Result<()> {
        let base = RocksTuning::baseline();
        assert_eq!(base, RocksTuning::preset("baseline")?);
        assert_eq!(base.block_bytes, BLOCK_BYTES);
        assert_eq!(base.block_cache_bytes, BLOCK_CACHE_BYTES);
        assert!(
            base.compress
                && !base.blobs
                && !base.direct_io
                && !base.universal
                && base.partition_filters
        );
        assert!(base.describe().contains("blob=off"));
        assert_eq!(base.background_jobs, BACKGROUND_JOBS);
        assert_eq!(base.target_file_bytes, TARGET_FILE_BYTES);
        assert!(base.share_resources);
        assert!(base.describe().contains("shared_resources=on"));

        let block_16k = RocksTuning::preset("block-16k")?;
        let block_256k = RocksTuning::preset("block-256k")?;
        let cache = RocksTuning::preset("cache-1g")?;
        assert_eq!(block_16k.block_bytes, 16 * 1024);
        assert_eq!(block_256k.block_bytes, 256 * 1024);
        assert_eq!(cache.block_cache_bytes, 1 << 30);
        assert!(!cache.share_resources);
        assert!(cache.describe().contains("shared_resources=off"));
        assert!(RocksTuning::preset("blobs")?.blobs);
        assert!(
            RocksTuning::preset("blobs")?
                .describe()
                .contains(&format!("blob=on blob_min_bytes={BLOB_MIN_BYTES}"))
        );
        assert!(!RocksTuning::preset("no-compress")?.compress);
        assert!(RocksTuning::preset("direct-io")?.direct_io);
        assert!(RocksTuning::preset("universal")?.universal);
        let full_filters = RocksTuning::preset("full-filters")?;
        assert!(!full_filters.partition_filters);
        assert_eq!(full_filters.block_cache_bytes, 128 * 1024 * 1024);
        assert_eq!(full_filters.cache_shard_bits, 0);
        assert!(!full_filters.share_resources);
        assert_eq!(base.cache_shard_bits, -1);
        assert_eq!(base.recycle_log_file_num, 4);
        assert!(base.use_fsync);
        assert_eq!(base.max_total_wal_size, 0);
        assert_eq!(base.small_file_compact_threshold, 0);
        assert!(!base.bloom_all_families);
        assert!(base.describe().contains("blooms=miss"));
        let all_blooms = RocksTuning::preset("all-blooms")?;
        assert!(all_blooms.bloom_all_families);
        assert!(all_blooms.partition_filters);
        assert!(all_blooms.describe().contains("blooms=all"));
        let no_recycle = RocksTuning::preset("no-wal-recycle")?;
        assert_eq!(no_recycle.recycle_log_file_num, 0);
        assert!(base.describe().contains("recycle_log_file_num=4"));
        assert!(base.describe().contains("use_fsync=true"));
        assert!(base.describe().contains("max_total_wal_size=0"));
        assert!(base.describe().contains("small_file_compact_threshold=0"));
        assert!(no_recycle.describe().contains("recycle_log_file_num=0"));
        assert!(no_recycle.describe().contains("filters=partitioned"));
        let fdatasync = RocksTuning::preset("fdatasync-wal")?;
        assert!(!fdatasync.use_fsync);
        assert_eq!(fdatasync.max_total_wal_size, 0);
        assert_eq!(fdatasync.recycle_log_file_num, 4);
        assert!(fdatasync.describe().contains("use_fsync=false"));
        let fdatasync_rotate = RocksTuning::preset("fdatasync-wal-rotate")?;
        assert!(!fdatasync_rotate.use_fsync);
        assert_eq!(fdatasync_rotate.max_total_wal_size, WAL_ROTATE_BYTES);
        assert_eq!(fdatasync_rotate.recycle_log_file_num, 4);
        assert_eq!(fdatasync_rotate.small_file_compact_threshold, 0);
        let fsync_rotate = RocksTuning::preset("fsync-wal-rotate")?;
        assert!(fsync_rotate.use_fsync);
        assert_eq!(fsync_rotate.max_total_wal_size, WAL_ROTATE_BYTES);
        assert_eq!(fsync_rotate.small_file_compact_threshold, 0);
        assert!(
            fsync_rotate
                .describe()
                .contains(&format!("max_total_wal_size={WAL_ROTATE_BYTES}"))
        );
        let rotate_compact = RocksTuning::preset("fdatasync-wal-rotate-compact")?;
        assert!(!rotate_compact.use_fsync);
        assert_eq!(rotate_compact.max_total_wal_size, WAL_ROTATE_BYTES);
        assert_eq!(
            rotate_compact.small_file_compact_threshold,
            SMALL_FILE_COMPACT_THRESHOLD
        );
        assert_eq!(rotate_compact.recycle_log_file_num, 4);
        assert!(rotate_compact.describe().contains(&format!(
            "small_file_compact_threshold={SMALL_FILE_COMPACT_THRESHOLD}"
        )));

        let mut names = Vec::new();
        for preset in RocksTuning::presets() {
            assert!(!names.contains(&preset.name), "duplicate {}", preset.name);
            names.push(preset.name);
            if preset.name == "baseline" {
                assert_eq!(preset.knob_diffs(base), 0);
            } else if preset.name == "full-filters" {
                assert_eq!(preset.knob_diffs(base), 3, "{}", preset.name);
            } else if preset.name == "fdatasync-wal-rotate" {
                assert_eq!(preset.knob_diffs(base), 2, "{}", preset.name);
            } else if preset.name == "fdatasync-wal-rotate-compact" {
                assert_eq!(preset.knob_diffs(base), 3, "{}", preset.name);
            } else {
                assert_eq!(preset.knob_diffs(base), 1, "{}", preset.name);
            }
        }
        assert!(RocksTuning::preset("missing").is_err());

        let overridden = cache.with_block_cache(4096);
        assert_eq!(overridden.name, "cache-1g");
        assert_eq!(overridden.block_cache_bytes, 4096);
        assert!(!overridden.share_resources);
        assert_eq!(overridden.knob_diffs(base), 1);
        let private = base.with_block_cache(4096);
        assert!(!private.share_resources);
        assert_eq!(private.block_cache_bytes, 4096);

        let dir = temp_dir("submodule_rocks_presets");
        for preset in RocksTuning::presets() {
            let path = dir.path().join(preset.name);
            let store = RocksSubmoduleStore::open_with(&path, preset)?;
            assert_eq!(store.uses_shared_resources(), preset.share_resources);
            let capacity = property_u64(&store.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?;
            if preset.share_resources {
                assert_eq!(capacity, irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES);
            } else {
                assert_eq!(capacity, preset.block_cache_bytes as u64);
            }
            drop(store);
        }
        let baseline_opts = persisted_options(&dir.path().join("baseline"))?;
        assert_contains(
            &baseline_opts,
            &[
                "recycle_log_file_num=4",
                "wal_recovery_mode=kPointInTimeRecovery",
                "partition_filters=true",
                "index_type=kTwoLevelIndexSearch",
                "pin_top_level_index_and_filter=true",
                "[CFOptions \"PlacementIntervals\"]",
                "[CFOptions \"DataRootPlacements\"]",
                "[CFOptions \"PendingBodyMigrations\"]",
                "[CFOptions \"ChunkMetaByOffset\"]",
                "[CFOptions \"ChunkOffsetsByContentHash\"]",
                "use_fsync=true",
                "max_total_wal_size=0",
            ],
        );
        assert_lacks(
            &baseline_opts,
            &[
                "ChunkDataPathByOffset",
                "TxPathByTxPathHash",
                "ChunkPathHashesByOffset",
            ],
        );
        assert_cf(
            &baseline_opts,
            CHUNK_META_BY_OFFSET_CF,
            "enable_blob_files",
            "false",
        );
        assert_cf(
            &baseline_opts,
            CHUNK_META_BY_OFFSET_CF,
            "enable_blob_garbage_collection",
            "true",
        );
        assert_cf(
            &baseline_opts,
            CHUNK_META_BY_OFFSET_CF,
            "blob_garbage_collection_age_cutoff",
            "1.000000",
        );
        assert_cf(
            &baseline_opts,
            "ChunkOffsetsByContentHash",
            "prefix_extractor",
            "rocksdb.FixedPrefix.32",
        );
        assert!(
            cf_option(
                &baseline_opts,
                "ChunkOffsetsByContentHash",
                "memtable_prefix_bloom_size_ratio"
            )
            .is_some_and(|value| value.starts_with("0.100")),
            "{baseline_opts}"
        );
        assert!(
            table_options_have_bloom(&baseline_opts, "ChunkOffsetsByContentHash"),
            "{baseline_opts}"
        );
        assert_eq!(
            table_option(
                &baseline_opts,
                "ChunkOffsetsByContentHash",
                "whole_key_filtering"
            )
            .as_deref(),
            Some("false"),
            "{baseline_opts}"
        );
        let blobs_opts = persisted_options(&dir.path().join("blobs"))?;
        let min_blob = BLOB_MIN_BYTES.to_string();
        assert_cf(
            &blobs_opts,
            CHUNK_META_BY_OFFSET_CF,
            "enable_blob_files",
            "true",
        );
        assert_cf(
            &blobs_opts,
            CHUNK_META_BY_OFFSET_CF,
            "min_blob_size",
            min_blob.as_str(),
        );
        assert_cf(
            &blobs_opts,
            CHUNK_META_BY_OFFSET_CF,
            "enable_blob_garbage_collection",
            "true",
        );
        assert_cf(
            &blobs_opts,
            CHUNK_META_BY_OFFSET_CF,
            "blob_garbage_collection_age_cutoff",
            "0.250000",
        );
        // An external block cache is not written to the OPTIONS file.
        // `cache_shard_bits` is checked on the preset above.
        let full = persisted_options(&dir.path().join("full-filters"))?;
        assert_contains(&full, &["partition_filters=false"]);
        assert_lacks(&full, &["index_type=kTwoLevelIndexSearch"]);
        assert_contains(
            &persisted_options(&dir.path().join("no-wal-recycle"))?,
            &[
                "recycle_log_file_num=0",
                "wal_recovery_mode=kPointInTimeRecovery",
                "partition_filters=true",
            ],
        );
        assert_contains(
            &persisted_options(&dir.path().join("fdatasync-wal"))?,
            &[
                "use_fsync=false",
                "max_total_wal_size=0",
                "recycle_log_file_num=4",
            ],
        );
        let wal_cap = format!("max_total_wal_size={WAL_ROTATE_BYTES}");
        assert_contains(
            &persisted_options(&dir.path().join("fdatasync-wal-rotate"))?,
            &["use_fsync=false", wal_cap.as_str()],
        );
        assert_contains(
            &persisted_options(&dir.path().join("fsync-wal-rotate"))?,
            &["use_fsync=true", wal_cap.as_str(), "recycle_log_file_num=4"],
        );
        assert_contains(
            &persisted_options(&dir.path().join("fdatasync-wal-rotate-compact"))?,
            &["use_fsync=false", wal_cap.as_str()],
        );
        Ok(())
    }

    /// An SST written with a bloom on every family still opens and reads when
    /// the next open leaves the always-hit families without a filter policy.
    /// The reverse open reads the same rows. New SSTs take the open's policy.
    /// `ChunkOffsetsByContentHash` keeps its prefix bloom on every open.
    #[test]
    fn bloom_scope_change_reopens_an_existing_db() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_bloom_scope");
        let path = dir.path().join("index");
        let offset = PartitionChunkOffset::from(4);
        let path_bytes = b"data-path".to_vec();
        let root = H256::repeat_byte(3);
        let job = PendingBodyMigration {
            data_root: root,
            data_size: 32,
            start_offset: RelativeChunkOffset(0),
            block_height: 1,
            attempts: 0,
        };
        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::preset("all-blooms")?)?;
            place_span(&store, 0, 7, root, 0, 1, 0, &[], None)?;
            store.update(|tx| {
                tx.add_full_data_path(offset, path_bytes.clone())?;
                tx.add_pending_body_migration(offset, &job)?;
                Ok(())
            })?;
            store.flush_memtables()?;
        }
        let written = persisted_options(&path)?;
        assert!(
            table_options_have_bloom(&written, CHUNK_META_BY_OFFSET_CF),
            "{written}"
        );
        assert!(
            table_options_have_bloom(&written, "PendingBodyMigrations"),
            "{written}"
        );
        assert!(
            table_options_have_bloom(&written, "ChunkOffsetsByContentHash"),
            "{written}"
        );

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            assert_eq!(
                store.view(|tx| tx.get_data_path_by_offset(offset))?,
                Some(path_bytes.clone())
            );
            assert_eq!(
                store.view(|tx| tx.get_pending_body_migration(offset))?,
                Some(job.clone())
            );
            // Touch the memtable so the following flush writes an SST under
            // the baseline policy. The rows above already live in the old SST.
            store.update(|tx| tx.add_full_data_path(PartitionChunkOffset::from(5), vec![1]))?;
            store.flush_memtables()?;
        }
        let reopened = persisted_options(&path)?;
        assert!(
            !table_options_have_bloom(&reopened, CHUNK_META_BY_OFFSET_CF),
            "{reopened}"
        );
        assert!(
            !table_options_have_bloom(&reopened, "PendingBodyMigrations"),
            "{reopened}"
        );
        assert!(
            table_options_have_bloom(&reopened, "ChunkOffsetsByContentHash"),
            "{reopened}"
        );

        let store = RocksSubmoduleStore::open_with(&path, RocksTuning::preset("all-blooms")?)?;
        assert_eq!(
            store.view(|tx| tx.get_data_path_by_offset(offset))?,
            Some(path_bytes)
        );
        assert_eq!(
            store.view(|tx| tx.get_pending_body_migration(offset))?,
            Some(job)
        );
        Ok(())
    }

    fn table_section<'a>(opts: &'a str, cf: &str) -> Option<&'a str> {
        let header = format!("[TableOptions/BlockBasedTable \"{cf}\"]");
        let start = opts.find(&header)? + header.len();
        let rest = &opts[start..];
        Some(match rest.find("\n[") {
            Some(end) => &rest[..end],
            None => rest,
        })
    }

    fn table_options_have_bloom(opts: &str, cf: &str) -> bool {
        table_section(opts, cf).is_some_and(|section| section.contains("filter_policy=bloomfilter"))
    }

    fn table_option(opts: &str, cf: &str, key: &str) -> Option<String> {
        let prefix = format!("{key}=");
        table_section(opts, cf)?
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix).map(ToString::to_string))
    }

    fn cf_option(opts: &str, cf: &str, key: &str) -> Option<String> {
        let header = format!("[CFOptions \"{cf}\"]");
        let start = opts.find(&header)? + header.len();
        let rest = &opts[start..];
        let section = match rest.find("\n[") {
            Some(end) => &rest[..end],
            None => rest,
        };
        let prefix = format!("{key}=");
        section
            .lines()
            .find_map(|line| line.trim().strip_prefix(&prefix).map(ToString::to_string))
    }

    /// Keeps the next opens in this thread from scheduling compaction.
    ///
    /// `SetOptions` after open loses the race with bottommost compaction.
    /// That job rewrites the blob SST and inlines it. The flag is read while
    /// the column options are built, including a later baseline open.
    struct HoldAutoCompaction;
    impl HoldAutoCompaction {
        fn enter() -> Self {
            OPEN_DISABLE_AUTO_COMPACTION.with(|flag| flag.set(true));
            Self
        }
    }
    impl Drop for HoldAutoCompaction {
        fn drop(&mut self) {
            OPEN_DISABLE_AUTO_COMPACTION.with(|flag| flag.set(false));
        }
    }

    /// Blob files from the `blobs` preset stay readable under baseline.
    /// Forced compaction inlines them. A second baseline open reads every
    /// path while blob values and new inline values are both present.
    #[test]
    fn blob_values_reopen_inline_and_compact_away() -> eyre::Result<()> {
        let _hold = HoldAutoCompaction::enter();
        const LEGACY_PATHS: u32 = 4_096;
        const INLINE_PATHS: u32 = 256;

        let dir = temp_dir("submodule_rocks_blob");
        let path = dir.path().join("index");
        let total = LEGACY_PATHS + INLINE_PATHS;
        let removed = [0_u32, LEGACY_PATHS];
        let root = H256::repeat_byte(7);

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::preset("blobs")?)?;
            place_span(&store, 0, total - 1, root, 0, 1, 0, &[], None)?;
            write_large_paths(&store, 0, LEGACY_PATHS)?;
            store.flush_memtables()?;
            assert!(
                store.blob_file_count(Cf::OffsetMeta)? > 0,
                "data paths did not reach blob files"
            );
        }

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            assert!(store.blob_file_count(Cf::OffsetMeta)? > 0);
            assert_data_paths(&store, LEGACY_PATHS, &[])?;

            write_large_paths(&store, LEGACY_PATHS, INLINE_PATHS)?;
            store.flush_memtables()?;
            assert!(
                store.blob_file_count(Cf::OffsetMeta)? > 0,
                "flush of inline values dropped blob files"
            );
            assert_data_paths(&store, total, &[])?;

            let stats = store.drop_presence_in_ranges(&[
                (
                    PartitionChunkOffset::from(removed[0]),
                    PartitionChunkOffset::from(removed[0]),
                ),
                (
                    PartitionChunkOffset::from(removed[1]),
                    PartitionChunkOffset::from(removed[1]),
                ),
            ])?;
            assert_eq!(stats.data_path_rows, 2);
            assert_eq!(stats.path_hash_rows, 0);
            assert_data_paths(&store, total, &removed)?;
        }

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            assert_data_paths(&store, total, &removed)?;
        }

        let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
        assert!(
            store.blob_file_count(Cf::OffsetMeta)? > 0,
            "reopen compacted blob files away"
        );
        store.compact_column_force(Cf::OffsetMeta)?;
        assert_eq!(store.blob_file_count(Cf::OffsetMeta)?, 0);
        assert_data_paths(&store, total, &removed)?;
        Ok(())
    }

    #[test]
    fn small_file_compact_stays_off_when_the_threshold_is_zero() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_compact_off");
        let store =
            RocksSubmoduleStore::open_with(dir.path().join("index"), RocksTuning::baseline())?;
        let rows = 20_u32;
        let files = plant_tiny_ssts(&store, rows)?;
        std::thread::sleep(Duration::from_millis(400));
        let stats = store.ingest_stats()?;
        assert_eq!(stats.small_file_compactions, 0);
        assert!(
            store.sst_file_count(Cf::Interval)? > 8,
            "threshold 0 must leave the tiny SSTs in place, started with {files}"
        );
        assert_planted_rows(&store, rows)
    }

    #[test]
    fn small_file_compact_merges_tiny_ssts_past_the_threshold() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_compact_on");
        let path = dir.path().join("index");
        let rows = 20_u32;
        let files = {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            let files = plant_tiny_ssts(&store, rows)?;
            assert_eq!(store.ingest_stats()?.small_file_compactions, 0);
            files
        };
        let tuning = RocksTuning {
            small_file_compact_threshold: 8,
            ..RocksTuning::baseline()
        };
        let store = RocksSubmoduleStore::open_with(&path, tuning)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let stats = store.ingest_stats()?;
            let after = store.sst_file_count(Cf::Interval)?;
            if stats.small_file_compactions > 0 && after < files {
                assert!(
                    stats.small_file_compact_files_after < stats.small_file_compact_files_before,
                    "before={} after={}",
                    stats.small_file_compact_files_before,
                    stats.small_file_compact_files_after
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                eyre::bail!(
                    "compact did not merge tiny SSTs: files {files} -> {after}, compactions {}",
                    stats.small_file_compactions
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_planted_rows(&store, rows)
    }

    /// Writes, flushes until a retired WAL is reused, then reopens.
    /// The last row is only in that WAL.
    #[test]
    fn wal_recycle_reopens_after_a_reused_log() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_wal_recycle");
        let path = dir.path().join("index");
        let preset = RocksTuning::baseline();
        let durable = [(1_u32, 11_u8), (2, 22), (3, 33), (4, 44)];
        let wal_only = (5_u32, 55_u8);
        {
            let store = RocksSubmoduleStore::open_with(&path, preset)?;
            for (offset, byte) in durable {
                put_offset(&store, offset, byte)?;
                store.flush_and_compact()?;
            }
            put_offset(&store, wal_only.0, wal_only.1)?;
        }
        let log = info_log(&path)?;
        assert!(
            log.contains("reusing log"),
            "expected a recycled WAL in the info log: {log}"
        );
        let store = RocksSubmoduleStore::open_with(&path, preset)?;
        for (offset, byte) in durable {
            assert_eq!(path_hashes_at(&store, offset)?, Some(hashes_for(byte)));
        }
        assert_eq!(
            path_hashes_at(&store, wal_only.0)?,
            Some(hashes_for(wal_only.1))
        );
        Ok(())
    }

    #[test]
    fn ingest_stats_reads_the_stall_line_and_zero_counters() -> eyre::Result<()> {
        assert_eq!(
            parse_cumulative_stall_micros("Cumulative stall: 00:00:00.000 H:M:S, 0.0 percent\n"),
            Some(0)
        );
        assert_eq!(
            parse_cumulative_stall_micros(
                "Cumulative writes: 1\nCumulative stall: 01:02:04.250 H:M:S, 9.0 percent\n"
            ),
            Some(3_724_250_000)
        );
        assert!(parse_cumulative_stall_micros("no stall line").is_none());

        let dir = temp_dir("submodule_rocks_stats");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        let stats = store.ingest_stats()?;
        assert_eq!(stats.write_stall_micros, 0);
        assert_eq!(stats.pending_compaction_bytes, 0);
        assert_eq!(stats.flushes, 0);
        assert_eq!(stats.running_flushes, 0);
        assert_eq!(stats.wal_syncs, 0);
        assert_eq!(stats.wal_backstop_syncs, 0);
        assert_eq!(stats.small_file_compactions, 0);
        assert_eq!(stats.small_file_compact_files_before, 0);
        assert_eq!(stats.small_file_compact_files_after, 0);
        drop(store);
        let index = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        let again = index.rocks_ingest_stats()?.expect("rocks counters");
        assert_eq!(again.write_stall_micros, 0);
        Ok(())
    }

    /// Compaction rows share the table. Only the high-priority count is flushes.
    #[test]
    fn high_priority_flush_count_ignores_compaction_rows() -> eyre::Result<()> {
        fn row(name: &str, count: u64) -> String {
            let mut tokens = vec![name.to_string(), "1/0".into(), "0.00".into(), "KB".into()];
            for _ in 0..13 {
                tokens.push("0.0".into());
            }
            tokens.push(count.to_string());
            tokens.extend([
                "0.000".into(),
                "1".into(),
                "0".into(),
                "0.0".into(),
                "0.0".into(),
            ]);
            tokens.join(" ")
        }
        let stats = format!(
            "{}\n{}\n{}\n{}\n",
            row("L0", 9),
            row("Sum", 9),
            row("Low", 4),
            row("High", 2),
        );
        assert_eq!(super::parse_high_priority_flush_count(&stats)?, 2);
        let both = format!("{}\n{}", row("High", 2), row("High", 1));
        assert_eq!(super::parse_high_priority_flush_count(&both)?, 3);
        assert_eq!(super::parse_high_priority_flush_count("no flush yet\n")?, 0);
        let broken = row("High", 1).replace(" 1 ", " not-a-count ");
        assert!(super::parse_high_priority_flush_count(&broken).is_err());
        Ok(())
    }

    /// `flushes` is flush jobs since open. A write that stays in the memtable
    /// does not count. A later compaction of those SSTs does not count either.
    /// The bench subtracts two samples, so the second flush has to move the
    /// counter by the same kind of delta.
    #[test]
    fn ingest_stats_counts_flushes_and_not_compactions() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_flush_count");
        let store = RocksSubmoduleStore::open(dir.path().join("index"))?;
        assert_eq!(store.ingest_stats()?.flushes, 0);
        put_offset(&store, 1, 7)?;
        assert_eq!(
            store.ingest_stats()?.flushes,
            0,
            "a memtable write is not a flush"
        );

        flush_every_family(&store)?;
        let flushed = store.ingest_stats()?.flushes;
        assert!(
            flushed > 0,
            "flush jobs stayed at 0 after flushing every column family\n{}",
            sample_cfstats(&store)
        );

        flush_every_family(&store)?;
        assert_eq!(
            store.ingest_stats()?.flushes,
            flushed,
            "flushing an empty memtable counted as another flush"
        );

        for cf in families() {
            let handle = cf_handle(&store.db, cf)?;
            store
                .db
                .compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        }
        assert_eq!(
            store.ingest_stats()?.flushes,
            flushed,
            "compaction counted as a flush"
        );

        put_offset(&store, 2, 8)?;
        let before = store.ingest_stats()?.flushes;
        assert_eq!(before, flushed);
        flush_every_family(&store)?;
        let after = store.ingest_stats()?.flushes;
        assert!(
            after > before,
            "second flush delta was 0 (before {before}, after {after})"
        );
        Ok(())
    }

    #[test]
    fn production_open_keeps_tables_and_tickers() -> eyre::Result<()> {
        let dir = temp_dir("submodule_rocks_stats");
        let bench = RocksSubmoduleStore::open_with_stats(
            dir.path().join("bench"),
            RocksTuning::baseline(),
        )?;
        let bg = bench.background()?;
        assert!(bg.no_file_opens.is_some());
        let bench_opts = persisted_options(&dir.path().join("bench"))?;
        assert!(bench_opts.contains("max_open_files=-1"), "{bench_opts}");
        assert!(
            bench_opts.contains("max_file_opening_threads=16"),
            "{bench_opts}"
        );
        assert!(
            bench_opts.contains("skip_stats_update_on_db_open=true"),
            "{bench_opts}"
        );
        let plain = RocksSubmoduleStore::open(dir.path().join("plain"))?;
        assert!(plain.background()?.no_file_opens.is_some());
        assert!(plain.uses_shared_resources());
        assert!(bench.uses_shared_resources());
        assert!(Arc::ptr_eq(
            plain.resources.as_ref().expect("shared"),
            bench.resources.as_ref().expect("shared"),
        ));
        assert_eq!(
            property_u64(&plain.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?,
            irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
        );
        plain.report_metrics();
        let plain_opts = persisted_options(&dir.path().join("plain"))?;
        assert!(plain_opts.contains("max_open_files=-1"), "{plain_opts}");
        assert!(
            plain_opts.contains("max_file_opening_threads=16"),
            "{plain_opts}"
        );
        assert!(
            !plain_opts.contains("skip_stats_update_on_db_open=true"),
            "{plain_opts}"
        );
        for opts in [&bench_opts, &plain_opts] {
            assert!(opts.contains("partition_filters=true"), "{opts}");
            assert!(opts.contains("index_type=kTwoLevelIndexSearch"), "{opts}");
            assert!(
                opts.contains("pin_top_level_index_and_filter=true"),
                "{opts}"
            );
            assert!(opts.contains("use_fsync=true"), "{opts}");
            assert!(opts.contains("max_total_wal_size=0"), "{opts}");
        }
        let private = RocksSubmoduleStore::open_with_block_cache(
            dir.path().join("private"),
            8 * 1024 * 1024,
        )?;
        assert!(!private.uses_shared_resources());
        assert_eq!(
            property_u64(&private.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?,
            8 * 1024 * 1024
        );
        Ok(())
    }

    #[test]
    fn resource_limits_reject_a_buffer_above_the_cache() {
        let mut config = DatabaseConfig::default();
        config.rocksdb_write_buffer_bytes = config.rocksdb_block_cache_bytes + 1;
        assert!(RocksResourceLimits::from_database(&config).is_err());
        config.rocksdb_write_buffer_bytes = 0;
        assert!(RocksResourceLimits::from_database(&config).is_err());
        config = DatabaseConfig::default();
        config.rocksdb_block_cache_bytes = 0;
        assert!(RocksResourceLimits::from_database(&config).is_err());
        config = DatabaseConfig::default();
        config.rocksdb_flush_threads = 0;
        assert!(RocksResourceLimits::from_database(&config).is_err());
        config = DatabaseConfig::default();
        config.rocksdb_compaction_threads = 0;
        assert!(RocksResourceLimits::from_database(&config).is_err());
        assert_eq!(
            RocksResourceLimits::from_database(&DatabaseConfig::default())
                .expect("production limits")
                .block_cache_bytes,
            irys_types::DEFAULT_ROCKSDB_BLOCK_CACHE_BYTES
        );
    }

    /// Forty databases on one explicit set stay inside that set.
    ///
    /// The column write buffer stays 64 MiB, so each empty family reserves
    /// about 1 MiB. Forty databases and seven families are about 280 MiB.
    /// That fits in the 512 MiB manager and in the 1 GiB cache. A smaller
    /// manager cannot hold the idle arenas.
    #[test]
    fn forty_databases_share_one_cache_and_write_buffer() -> eyre::Result<()> {
        let limits = RocksResourceLimits::production();
        let resources = RocksResources::new(limits)?;
        let before = rocksdb_thread_count();
        let dir = temp_dir("submodule_rocks_shared");
        let mut stores = Vec::with_capacity(40);
        for index in 0..40 {
            let path = dir.path().join(format!("db-{index}"));
            stores.push(RocksSubmoduleStore::open_with_resources(
                path,
                Arc::clone(&resources),
            )?);
        }
        let after = rocksdb_thread_count();
        if cfg!(target_os = "linux") {
            assert!(
                before >= 6,
                "expected the flush and compaction pools, saw {before}"
            );
            assert_eq!(
                after, before,
                "rocksdb threads changed across 40 opens: {before} -> {after}"
            );
        }
        let mut memtables = 0_u64;
        for store in &stores {
            assert!(store.uses_shared_resources());
            assert!(Arc::ptr_eq(
                store.resources.as_ref().expect("shared"),
                &resources
            ));
            let capacity = property_u64(&store.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?;
            let usage = property_u64(&store.db, rocksdb::properties::BLOCK_CACHE_USAGE)?;
            assert_eq!(capacity, limits.block_cache_bytes);
            assert!(usage <= capacity, "cache usage {usage} exceeds {capacity}");
            memtables = memtables.saturating_add(property_u64(
                &store.db,
                rocksdb::properties::CUR_SIZE_ALL_MEM_TABLES,
            )?);
            store.report_metrics();
        }
        assert!(
            memtables <= limits.write_buffer_bytes,
            "memtables {memtables} exceed {}",
            limits.write_buffer_bytes
        );
        assert!(
            resources.write_buffer_usage() <= resources.write_buffer_capacity(),
            "write buffer {} exceeds {}",
            resources.write_buffer_usage(),
            resources.write_buffer_capacity()
        );
        assert!(resources.block_cache_usage() <= resources.block_cache_capacity());
        assert_eq!(resources.write_buffer_capacity(), limits.write_buffer_bytes);
        Ok(())
    }

    /// The binding creates a limiter inside each open. Two opens must both succeed.
    #[test]
    fn rate_limit_opens_one_limiter_per_database() -> eyre::Result<()> {
        let limits = RocksResourceLimits {
            block_cache_bytes: 8 * 1024 * 1024,
            write_buffer_bytes: 4 * 1024 * 1024,
            flush_threads: irys_types::DEFAULT_ROCKSDB_FLUSH_THREADS,
            compaction_threads: irys_types::DEFAULT_ROCKSDB_COMPACTION_THREADS,
            rate_limit_bytes_per_sec: 1024 * 1024,
        };
        let resources = RocksResources::new(limits)?;
        let dir = temp_dir("submodule_rocks_rate");
        let first =
            RocksSubmoduleStore::open_with_resources(dir.path().join("a"), Arc::clone(&resources))?;
        let second =
            RocksSubmoduleStore::open_with_resources(dir.path().join("b"), Arc::clone(&resources))?;
        assert!(first.uses_shared_resources());
        assert!(second.uses_shared_resources());
        assert_eq!(
            property_u64(&first.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?,
            limits.block_cache_bytes
        );
        assert_eq!(
            property_u64(&second.db, rocksdb::properties::BLOCK_CACHE_CAPACITY)?,
            limits.block_cache_bytes
        );
        Ok(())
    }

    fn rocksdb_thread_count() -> usize {
        let Ok(tasks) = fs::read_dir("/proc/self/task") else {
            return 0;
        };
        tasks
            .flatten()
            .filter(|task| {
                fs::read_to_string(task.path().join("comm"))
                    .is_ok_and(|comm| comm.starts_with("rocksdb:"))
            })
            .count()
    }

    fn hashes_for(byte: u8) -> ChunkPathHashes {
        // No proof is stored, so the data-path hash stays empty. The tx-path
        // hash is the placement data root.
        ChunkPathHashes {
            data_path_hash: None,
            tx_path_hash: Some(H256::repeat_byte(byte.wrapping_add(1))),
        }
    }

    fn plant_tiny_ssts(store: &RocksSubmoduleStore, rows: u32) -> eyre::Result<u64> {
        store.disable_auto_compaction()?;
        for offset in 0..rows {
            put_offset(store, offset, offset as u8)?;
            store.flush_memtables()?;
        }
        let files = store.sst_file_count(Cf::Interval)?;
        assert!(
            files > 8,
            "expected many tiny SSTs after per-row flushes, got {files}"
        );
        Ok(files)
    }

    fn assert_planted_rows(store: &RocksSubmoduleStore, rows: u32) -> eyre::Result<()> {
        for offset in 0..rows {
            assert_eq!(
                path_hashes_at(store, offset)?,
                Some(hashes_for(offset as u8))
            );
        }
        Ok(())
    }

    fn assert_contains(opts: &str, needles: &[&str]) {
        for needle in needles {
            assert!(opts.contains(needle), "missing {needle} in {opts}");
        }
    }

    fn assert_lacks(opts: &str, needles: &[&str]) {
        for needle in needles {
            assert!(!opts.contains(needle), "unexpected {needle} in {opts}");
        }
    }

    fn assert_cf(opts: &str, cf: &str, key: &str, want: &str) {
        assert_eq!(
            cf_option(opts, cf, key).as_deref(),
            Some(want),
            "{key} in {opts}"
        );
    }

    fn sample_cfstats(store: &RocksSubmoduleStore) -> String {
        let Some(cf) = families().next() else {
            return String::new();
        };
        let Ok(handle) = cf_handle(&store.db, cf) else {
            return String::new();
        };
        store
            .db
            .property_value_cf(handle, "rocksdb.cfstats-no-file-histogram")
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Direct `flush_cf`. Not the store's flush helpers, so a counter bumped
    /// only around those helpers stays at 0.
    fn flush_every_family(store: &RocksSubmoduleStore) -> eyre::Result<()> {
        for cf in families() {
            let handle = cf_handle(&store.db, cf)?;
            store
                .db
                .flush_cf(&handle)
                .map_err(|error| eyre::eyre!("flush column family: {error}"))?;
        }
        Ok(())
    }

    fn put_offset(store: &RocksSubmoduleStore, offset: u32, byte: u8) -> eyre::Result<()> {
        let hashes = hashes_for(byte);
        let root = hashes.tx_path_hash.expect("placement root");
        store.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.set_path_hashes_by_offset(PartitionChunkOffset::from(offset), hashes)
        })
    }

    /// Path bytes at the blob threshold. The offset is in the first four
    /// bytes so two rows cannot compare equal.
    fn large_path(offset: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; BLOB_MIN_BYTES as usize];
        bytes[..4].copy_from_slice(&offset.to_le_bytes());
        for (index, byte) in bytes.iter_mut().enumerate().skip(4) {
            *byte = (index as u8).wrapping_add(offset as u8);
        }
        bytes
    }

    fn write_large_paths(store: &RocksSubmoduleStore, start: u32, count: u32) -> eyre::Result<()> {
        const BATCH: u32 = 512;
        let end = start + count;
        let mut offset = start;
        while offset < end {
            let batch_end = offset.saturating_add(BATCH).min(end);
            store.update(|tx| {
                for raw in offset..batch_end {
                    tx.add_full_data_path(PartitionChunkOffset::from(raw), large_path(raw))?;
                }
                Ok(())
            })?;
            offset = batch_end;
        }
        Ok(())
    }

    fn assert_data_paths(
        store: &RocksSubmoduleStore,
        end: u32,
        removed: &[u32],
    ) -> eyre::Result<()> {
        store.view(|tx| {
            for raw in 0..end {
                let got = tx.get_data_path_by_offset(PartitionChunkOffset::from(raw))?;
                if removed.contains(&raw) {
                    assert!(got.is_none(), "offset {raw} should be gone");
                } else {
                    assert_eq!(
                        got.as_deref(),
                        Some(large_path(raw).as_slice()),
                        "offset {raw}"
                    );
                }
            }
            Ok(())
        })
    }

    fn path_hashes_at(
        store: &RocksSubmoduleStore,
        offset: u32,
    ) -> eyre::Result<Option<ChunkPathHashes>> {
        store.view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(offset)))
    }

    fn info_log(path: &Path) -> eyre::Result<String> {
        let mut text = String::new();
        for entry in fs::read_dir(path).wrap_err("read db dir")? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "LOG" || name.starts_with("LOG.old") {
                text.push_str(&fs::read_to_string(entry.path()).wrap_err("read info log")?);
            }
        }
        Ok(text)
    }

    /// The newest `OPTIONS-<number>` file. RocksDB keeps the previous one too,
    /// and `read_dir` order is not creation order.
    fn persisted_options(path: &Path) -> eyre::Result<String> {
        let mut newest: Option<(u64, PathBuf)> = None;
        for entry in fs::read_dir(path).wrap_err("read db dir")? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(number) = name
                .to_string_lossy()
                .strip_prefix("OPTIONS-")
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            if newest.as_ref().is_none_or(|(seen, _)| number > *seen) {
                newest = Some((number, entry.path()));
            }
        }
        let (_, file) =
            newest.ok_or_else(|| eyre::eyre!("no OPTIONS file in {}", path.display()))?;
        fs::read_to_string(file).wrap_err("read OPTIONS")
    }

    fn dir_names(path: &Path) -> eyre::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(path).wrap_err("read db dir")? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn manifest_bytes(path: &Path) -> eyre::Result<Vec<u8>> {
        let current = fs::read_to_string(path.join("CURRENT")).wrap_err("read CURRENT")?;
        let name = current.trim();
        fs::read(path.join(name)).wrap_err_with(|| format!("read {name}"))
    }

    fn copy_tree(from: &Path, to: &Path) -> eyre::Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from).wrap_err("read tree")? {
            let entry = entry?;
            let dest = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &dest)?;
            } else {
                fs::copy(entry.path(), &dest)?;
            }
        }
        Ok(())
    }

    fn placement_row_count(path: &Path) -> eyre::Result<usize> {
        let db = open_db_read_only(path)?;
        let handle = db
            .cf_handle("PlacementIntervals")
            .ok_or_else(|| eyre::eyre!("PlacementIntervals missing"))?;
        let rows = db
            .iterator_cf(handle, IteratorMode::Start)
            .collect::<Result<Vec<_>, rocksdb::Error>>()
            .map_err(|err| eyre::eyre!("scan placement intervals: {err}"))?;
        Ok(rows.len())
    }

    /// Shutdown flush records the memtable in the moved directory's manifest
    /// and writes the SST into the empty directory, which is then deleted.
    /// The moved manifest must still open, and only the flushed row is durable.
    #[test]
    fn refused_close_keeps_flushed_rows_in_the_moved_directory() -> eyre::Result<()> {
        let root = temp_dir("refused-close-moved");
        let rocks = root.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        // A background compaction writes the new SST by path and the manifest
        // edit through the open fd. That is a separate race. This test is the
        // shutdown flush of the refused close.
        store.disable_auto_compaction()?;
        put_offset(&store, 0, 1)?;
        flush_every_family(&store)?;
        let manifest = manifest_bytes(&rocks)?;
        let names = dir_names(&rocks)?;
        put_offset(&store, 2, 2)?;
        let moved = root.path().join("rocks-moved");
        fs::rename(&rocks, &moved)?;
        fs::create_dir(&rocks)?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000099\n")?;
        fs::write(rocks.join("000099.log"), b"replacement-log")?;
        fs::write(rocks.join("LOCK"), b"")?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let err = store.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("replaced"), "{text}");
        assert_eq!(
            manifest_bytes(&moved)?,
            manifest,
            "refused close recorded a flush in the moved manifest"
        );
        assert_eq!(
            dir_names(&moved)?,
            names,
            "moved directory gained or lost files"
        );
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000099\n");
        assert_eq!(fs::read(rocks.join("000099.log"))?, b"replacement-log");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!rocks.join("SCHEMA").is_file());
        assert_eq!(
            dir_names(&rocks)?,
            vec![
                "000099.log".to_owned(),
                "CURRENT".to_owned(),
                "KEEP".to_owned(),
                "LOCK".to_owned(),
            ]
        );
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        let copy = root.path().join("rocks-copy");
        copy_tree(&moved, &copy)?;
        assert_eq!(placement_row_count(&copy)?, 1);
        Ok(())
    }

    /// Removing `LOCK` on the directory this database still owns refuses the
    /// seal, restores that directory, and keeps the flushed manifest.
    #[test]
    fn refused_close_of_the_same_directory_keeps_flushed_rows() -> eyre::Result<()> {
        let root = temp_dir("refused-close-same");
        let rocks = root.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        store.disable_auto_compaction()?;
        put_offset(&store, 0, 1)?;
        flush_every_family(&store)?;
        put_offset(&store, 2, 2)?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        fs::remove_file(rocks.join("LOCK"))?;
        let manifest = manifest_bytes(&rocks)?;
        let names = dir_names(&rocks)?;
        let err = store.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("replaced"), "{text}");
        assert_eq!(
            manifest_bytes(&rocks)?,
            manifest,
            "refused close recorded a flush in the restored manifest"
        );
        assert_eq!(dir_names(&rocks)?, names);
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!rocks.join("SCHEMA").is_file());
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        let copy = root.path().join("rocks-copy");
        copy_tree(&rocks, &copy)?;
        assert_eq!(placement_row_count(&copy)?, 1);
        Ok(())
    }

    /// The scratch marker cannot be created. The open database must not flush
    /// into the directory that is still at the path.
    #[test]
    fn marker_write_failure_does_not_publish_into_the_replacement() -> eyre::Result<()> {
        let root = temp_dir("refused-close-marker");
        let rocks = root.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        store.disable_auto_compaction()?;
        put_offset(&store, 0, 1)?;
        flush_every_family(&store)?;
        let manifest = manifest_bytes(&rocks)?;
        let names = dir_names(&rocks)?;
        put_offset(&store, 2, 2)?;
        let moved = root.path().join("rocks-moved");
        fs::rename(&rocks, &moved)?;
        fs::create_dir(&rocks)?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000099\n")?;
        fs::write(rocks.join("000099.log"), b"replacement-log")?;
        fs::write(rocks.join("LOCK"), b"")?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        // The rename of the marker tmp lands on this directory and fails.
        fs::create_dir(root.path().join(".rocks.close-scratch-marker"))?;
        let err = store.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("close scratch marker"),
            "marker failure must surface before a flush: {text}"
        );
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000099\n");
        assert_eq!(fs::read(rocks.join("000099.log"))?, b"replacement-log");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!rocks.join("SCHEMA").is_file());
        assert_eq!(
            dir_names(&rocks)?,
            vec![
                "000099.log".to_owned(),
                "CURRENT".to_owned(),
                "KEEP".to_owned(),
                "LOCK".to_owned(),
            ]
        );
        assert_eq!(
            manifest_bytes(&moved)?,
            manifest,
            "marker failure recorded a flush in the moved manifest"
        );
        assert_eq!(dir_names(&moved)?, names);
        let copy = root.path().join("rocks-copy");
        copy_tree(&moved, &copy)?;
        assert_eq!(placement_row_count(&copy)?, 1);
        Ok(())
    }

    fn plant_replacement(rocks: &Path) -> eyre::Result<()> {
        fs::create_dir(rocks)?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000099\n")?;
        fs::write(rocks.join("000099.log"), b"replacement-log")?;
        fs::write(rocks.join("LOCK"), b"")?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        Ok(())
    }

    fn schedule_memtable_flush(store: &RocksSubmoduleStore) -> eyre::Result<()> {
        let mut opts = rocksdb::FlushOptions::default();
        // The write-buffer path schedules this and returns. It does not wait.
        opts.set_wait(false);
        for cf in families() {
            let handle = cf_handle(&store.db, cf)?;
            store
                .db
                .flush_cf_opt(&handle, &opts)
                .map_err(|err| eyre::eyre!("schedule flush: {err}"))?;
        }
        Ok(())
    }

    fn replacement_names() -> Vec<String> {
        vec![
            "000099.log".to_owned(),
            "CURRENT".to_owned(),
            "KEEP".to_owned(),
            "LOCK".to_owned(),
        ]
    }

    /// A queued flush that publishes shows up inside this window.
    fn assert_replacement_stays_planted(rocks: &Path) -> eyre::Result<()> {
        let started = Instant::now();
        loop {
            assert_eq!(
                dir_names(rocks)?,
                replacement_names(),
                "replacement gained flush files"
            );
            assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000099\n");
            if started.elapsed() >= Duration::from_millis(800) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    struct QueuedFlush {
        root: irys_testing_utils::utils::tempfile::TempDir,
        rocks: PathBuf,
        store: RocksSubmoduleStore,
    }

    /// One durable row, one memtable row, and a non-blocking flush of that row.
    /// The flush is queued while the open directory is still the original.
    fn open_with_queued_flush(prefix: &str) -> eyre::Result<QueuedFlush> {
        let root = temp_dir(prefix);
        let rocks = root.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        put_offset(&store, 0, 1)?;
        flush_every_family(&store)?;
        put_offset(&store, 2, 2)?;
        schedule_memtable_flush(&store)?;
        Ok(QueuedFlush { root, rocks, store })
    }

    /// The empty-directory arm. The flush is queued while the lock protects
    /// the directory. The swap runs after that wait. The replacement must stay
    /// the planted files, and both rows must open from the moved directory.
    #[test]
    fn queued_flush_drained_before_swap_does_not_publish_into_the_replacement() -> eyre::Result<()>
    {
        reset_background_drains();
        let prepared = open_with_queued_flush("queued-flush-swap")?;
        let rocks = prepared.rocks.clone();
        let moved = prepared.root.path().join("rocks-moved");
        let rocks_hook = rocks.clone();
        let moved_hook = moved.clone();
        let _hook = set_after_protected_drain(move || {
            fs::rename(&rocks_hook, &moved_hook).expect("rename open directory");
            plant_replacement(&rocks_hook).expect("plant replacement");
        });
        let err = prepared.store.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("replaced"), "{text}");
        assert_replacement_stays_planted(&rocks)?;
        assert!(!rocks.join("SCHEMA").is_file());
        assert!(
            !prepared
                .root
                .path()
                .join(".rocks.close-scratch-marker")
                .exists()
        );
        let copy = prepared.root.path().join("rocks-copy");
        copy_tree(&moved, &copy)?;
        assert_eq!(
            placement_row_count(&copy)?,
            2,
            "queued flush did not finish in the original directory"
        );
        assert_eq!(
            background_drains(),
            1,
            "refused close drained more than the protected wait"
        );
        Ok(())
    }

    /// The scratch-marker arm. Same queued flush, then the marker path is a
    /// directory so the marker rename fails. Abandon must not publish.
    #[test]
    fn queued_flush_drained_before_marker_failure_does_not_publish() -> eyre::Result<()> {
        reset_background_drains();
        let prepared = open_with_queued_flush("queued-flush-marker")?;
        let rocks = prepared.rocks.clone();
        let moved = prepared.root.path().join("rocks-moved");
        let rocks_hook = rocks.clone();
        let moved_hook = moved.clone();
        let marker = prepared.root.path().join(".rocks.close-scratch-marker");
        let _hook = set_after_protected_drain(move || {
            fs::rename(&rocks_hook, &moved_hook).expect("rename open directory");
            plant_replacement(&rocks_hook).expect("plant replacement");
            fs::create_dir(&marker).expect("marker path is a directory");
        });
        let err = prepared.store.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("close scratch marker"),
            "marker failure must surface: {text}"
        );
        assert_replacement_stays_planted(&rocks)?;
        assert!(!rocks.join("SCHEMA").is_file());
        let copy = prepared.root.path().join("rocks-copy");
        copy_tree(&moved, &copy)?;
        assert_eq!(
            placement_row_count(&copy)?,
            2,
            "queued flush did not finish in the original directory"
        );
        assert_eq!(
            background_drains(),
            1,
            "marker failure drained more than the protected wait"
        );
        Ok(())
    }

    /// A flush scheduled after the directory is already swapped cannot be
    /// waited on: that wait publishes. This seal must not drain.
    #[test]
    fn flush_queued_after_the_swap_is_not_drained() -> eyre::Result<()> {
        reset_background_drains();
        let prepared = open_with_queued_flush("queued-flush-after-swap")?;
        let moved = prepared.root.path().join("rocks-moved");
        fs::rename(&prepared.rocks, &moved)?;
        plant_replacement(&prepared.rocks)?;
        schedule_memtable_flush(&prepared.store)?;
        let err = prepared.store.seal_and_stamp().unwrap_err();
        assert!(format!("{err:#}").contains("replaced"), "{err:?}");
        assert_eq!(
            background_drains(),
            0,
            "draining after the swap publishes the queued flush"
        );
        Ok(())
    }

    /// The order the refuse path must still allow: the directory stays, the
    /// queued flush is part of the sealed index, and SCHEMA is written.
    #[test]
    fn queued_flush_still_seals_when_the_directory_stays() -> eyre::Result<()> {
        reset_background_drains();
        let prepared = open_with_queued_flush("queued-flush-seal")?;
        prepared.store.seal_and_stamp()?;
        assert_eq!(
            background_drains(),
            2,
            "a protected seal drains before the flush and after it"
        );
        assert_eq!(
            fs::read_to_string(prepared.rocks.join(SCHEMA_FILE))?,
            SUBMODULE_SCHEMA_ROCKS_V1
        );
        assert!(
            !prepared
                .root
                .path()
                .join(".rocks.close-scratch-marker")
                .exists()
        );
        let copy = prepared.root.path().join("rocks-copy");
        copy_tree(&prepared.rocks, &copy)?;
        assert_eq!(placement_row_count(&copy)?, 2);
        Ok(())
    }

    /// Several L0 flushes with auto compaction left on. Wait until the manifest
    /// stops changing, then swap. A later change is a publish from the refused
    /// close, not a compaction that finished in the original directory.
    #[test]
    fn probe_l0_compaction_does_not_publish_into_the_replacement() -> eyre::Result<()> {
        let root = temp_dir("probe-l0-compact");
        let rocks = root.path().join("rocks");
        fs::create_dir(&rocks)?;
        let store = RocksSubmoduleStore::open_converting(&rocks)?;
        for offset in 0..8 {
            put_offset(&store, offset, offset as u8)?;
            flush_every_family(&store)?;
        }
        let settled = wait_until_manifest_settles(&store, &rocks)?;
        let names = dir_names(&rocks)?;
        let moved = root.path().join("rocks-moved");
        fs::rename(&rocks, &moved)?;
        plant_replacement(&rocks)?;
        assert_eq!(
            manifest_bytes(&moved)?,
            settled,
            "manifest changed before seal"
        );
        let err = store.seal_and_stamp().unwrap_err();
        assert!(format!("{err:#}").contains("replaced"), "{err:?}");
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(800) {
            assert_eq!(
                manifest_bytes(&moved)?,
                settled,
                "compaction edited the moved manifest after seal; replacement={:?} moved={:?}",
                dir_names(&rocks)?,
                dir_names(&moved)?
            );
            assert_eq!(
                dir_names(&moved)?,
                names,
                "moved directory gained or lost files"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000099\n");
        assert_eq!(
            dir_names(&rocks)?,
            replacement_names(),
            "replacement gained files"
        );
        Ok(())
    }

    fn background_work_is_idle(store: &RocksSubmoduleStore) -> eyre::Result<bool> {
        let mut busy = property_u64(&store.db, "rocksdb.num-running-compactions")?
            .saturating_add(property_u64(&store.db, "rocksdb.num-running-flushes")?);
        for cf in families() {
            let handle = cf_handle(&store.db, cf)?;
            for name in [
                "rocksdb.compaction-pending",
                "rocksdb.mem-table-flush-pending",
            ] {
                busy = busy.saturating_add(int_property_cf(&store.db, handle, name)?);
            }
        }
        Ok(busy == 0)
    }

    /// Returns the manifest once no flush or compaction is running or queued
    /// and the manifest bytes are unchanged across a quiet interval.
    fn wait_until_manifest_settles(
        store: &RocksSubmoduleStore,
        path: &Path,
    ) -> eyre::Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if background_work_is_idle(store)? {
                let before = manifest_bytes(path)?;
                std::thread::sleep(Duration::from_millis(200));
                if background_work_is_idle(store)? && manifest_bytes(path)? == before {
                    return Ok(before);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eyre::bail!("background work still running before the swap")
    }

    /// Proofs at the start of each of three sub-spans, plus one just past the span.
    fn seed_three_subspans(store: &RocksSubmoduleStore) -> eyre::Result<(u32, u32)> {
        let limit = DELETE_SUBRANGE_OFFSETS;
        let span_end = 3 * limit - 1;
        let neighbor = 3 * limit;
        place_span(
            store,
            0,
            neighbor,
            H256::repeat_byte(4),
            0,
            1,
            0,
            &[0, limit, 2 * limit, neighbor],
            None,
        )?;
        Ok((span_end, neighbor))
    }

    fn content_offsets(store: &RocksSubmoduleStore) -> eyre::Result<Vec<u32>> {
        let offsets = store.view(|tx| tx.offsets_for_content_hash(H256::repeat_byte(0)))?;
        Ok(offsets.into_iter().map(|offset| offset.0).collect())
    }

    /// A span wider than one sub-span deletes the same rows as one shot of
    /// that span. The neighbor just past the span stays.
    #[test]
    fn subspan_delete_matches_one_shot() -> eyre::Result<()> {
        let limit = DELETE_SUBRANGE_OFFSETS;
        let dropped_chunked = temp_dir("submodule_rocks_subspan_drop_chunked");
        let dropped_once = temp_dir("submodule_rocks_subspan_drop_once");
        let chunked = RocksSubmoduleStore::open(dropped_chunked.path().join("index"))?;
        let once = RocksSubmoduleStore::open(dropped_once.path().join("index"))?;
        let (span_end, neighbor) = seed_three_subspans(&chunked)?;
        seed_three_subspans(&once)?;
        let span = (
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(span_end),
        );
        let chunked_stats = chunked.drop_presence_in_ranges(&[span])?;
        let once_stats = once.delete_presence_ranges_one(&[span])?;
        assert_eq!(chunked_stats, once_stats);
        assert_eq!(chunked_stats.data_path_rows, 3);
        assert_eq!(content_offsets(&chunked)?, content_offsets(&once)?);
        assert_eq!(content_offsets(&chunked)?, vec![neighbor]);
        assert_content_index(&chunked)?;
        assert_content_index(&once)?;

        let retained_chunked = temp_dir("submodule_rocks_subspan_retain_chunked");
        let retained_once = temp_dir("submodule_rocks_subspan_retain_once");
        let chunked = RocksSubmoduleStore::open(retained_chunked.path().join("index"))?;
        let once = RocksSubmoduleStore::open(retained_once.path().join("index"))?;
        seed_three_subspans(&chunked)?;
        seed_three_subspans(&once)?;
        put_checksums(&chunked, &[1, limit + 1])?;
        put_checksums(&once, &[1, limit + 1])?;
        let chunked_stats = chunked.retain_entropy_crc_rows(&[span])?;
        let once_stats = once.retain_entropy_crc_rows_one(&[span])?;
        assert_eq!(chunked_stats, once_stats);
        assert_eq!(chunked_stats.data_path_rows, 3);
        assert_eq!(content_offsets(&chunked)?, vec![neighbor]);
        assert_eq!(content_offsets(&chunked)?, content_offsets(&once)?);
        for offset in [1_u32, limit + 1] {
            let meta = chunked
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                .expect("checksum row");
            assert!(meta.proof.is_none() && meta.crc.is_some());
            let other = once
                .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(offset)))?
                .expect("one-shot checksum row");
            assert_eq!(meta.crc, other.crc);
            assert!(other.proof.is_none());
        }
        assert_content_index(&chunked)?;
        assert_content_index(&once)?;
        Ok(())
    }

    /// Three offset chunks must not share one synced delete. A proof in each
    /// chunk makes that chunk write. One commit means the whole span was one batch.
    #[test]
    fn presence_delete_commits_once_per_offset_subrange() -> eyre::Result<()> {
        let limit = DELETE_SUBRANGE_OFFSETS;
        let span_end = 3 * limit - 1;
        let proofs = [0_u32, limit, 2 * limit];
        let span = (
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(span_end),
        );

        let dropped = temp_dir("submodule_rocks_subrange_drop_commits");
        let store = RocksSubmoduleStore::open(dropped.path().join("index"))?;
        place_span(
            &store,
            0,
            span_end,
            H256::repeat_byte(4),
            0,
            1,
            0,
            &proofs,
            None,
        )?;
        let before = store.wal_sync_count();
        store.drop_presence_in_ranges(&[span])?;
        let commits = store.wal_sync_count() - before;
        assert_eq!(
            commits, 3,
            "presence delete of 3 * {limit} offsets used {commits} commits"
        );

        let retained = temp_dir("submodule_rocks_subrange_retain_commits");
        let store = RocksSubmoduleStore::open(retained.path().join("index"))?;
        place_span(
            &store,
            0,
            span_end,
            H256::repeat_byte(4),
            0,
            1,
            0,
            &proofs,
            None,
        )?;
        let before = store.wal_sync_count();
        store.retain_entropy_crc_rows(&[span])?;
        let commits = store.wal_sync_count() - before;
        assert_eq!(
            commits, 3,
            "entropy retain of 3 * {limit} offsets used {commits} commits"
        );
        Ok(())
    }

    /// Many small disjoint spans share one commit. Spans whose offset count
    /// exceeds [`DELETE_SUBRANGE_OFFSETS`] take another.
    #[test]
    fn disjoint_presence_spans_share_a_commit_until_the_offset_limit() -> eyre::Result<()> {
        let limit = DELETE_SUBRANGE_OFFSETS;
        let small: Vec<_> = (0..8_u32)
            .map(|index| {
                let offset = index * 2;
                (
                    PartitionChunkOffset::from(offset),
                    PartitionChunkOffset::from(offset),
                )
            })
            .collect();
        let seed = |store: &RocksSubmoduleStore,
                    spans: &[(PartitionChunkOffset, PartitionChunkOffset)]|
         -> eyre::Result<()> {
            for (index, (start, end)) in spans.iter().copied().enumerate() {
                place_span(
                    store,
                    start.0,
                    end.0,
                    H256::repeat_byte(u8::try_from(index).unwrap_or(1).saturating_add(1)),
                    0,
                    1,
                    u32::try_from(index).unwrap_or(0),
                    &[start.0],
                    None,
                )?;
            }
            Ok(())
        };

        let dropped = temp_dir("submodule_rocks_disjoint_drop_commits");
        let store = RocksSubmoduleStore::open(dropped.path().join("index"))?;
        seed(&store, &small)?;
        let before = store.wal_sync_count();
        store.drop_presence_in_ranges(&small)?;
        let commits = store.wal_sync_count() - before;
        assert_eq!(commits, 1, "8 one-offset spans used {commits} drop commits");

        let retained = temp_dir("submodule_rocks_disjoint_retain_commits");
        let store = RocksSubmoduleStore::open(retained.path().join("index"))?;
        seed(&store, &small)?;
        let before = store.wal_sync_count();
        store.retain_entropy_crc_rows(&small)?;
        let commits = store.wal_sync_count() - before;
        assert_eq!(
            commits, 1,
            "8 one-offset spans used {commits} retain commits"
        );

        let wide_start = limit + 10;
        let wide = [
            (
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(limit - 1),
            ),
            (
                PartitionChunkOffset::from(wide_start),
                PartitionChunkOffset::from(wide_start + limit - 1),
            ),
        ];
        let wide_dropped = temp_dir("submodule_rocks_disjoint_wide_commits");
        let store = RocksSubmoduleStore::open(wide_dropped.path().join("index"))?;
        seed(&store, &wide)?;
        let before = store.wal_sync_count();
        store.drop_presence_in_ranges(&wide)?;
        let commits = store.wal_sync_count() - before;
        assert!(
            commits > 1,
            "spans totaling more than {limit} offsets used {commits} commits"
        );
        Ok(())
    }

    /// The first sub-span is durable and the rest is still present. Reopen,
    /// then the same delete open reconcile runs finishes the span.
    #[test]
    fn crash_after_first_subspan_reconciles_on_reopen() -> eyre::Result<()> {
        let limit = DELETE_SUBRANGE_OFFSETS;
        let first = (
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(limit - 1),
        );

        let dropped = temp_dir("submodule_rocks_subspan_crash_drop");
        let path = dropped.path().join("index");
        let (span_end, neighbor) = {
            let store = RocksSubmoduleStore::open(&path)?;
            let seeded = seed_three_subspans(&store)?;
            store.delete_presence_ranges_one(&[first])?;
            drop(store);
            seeded
        };
        let store = RocksSubmoduleStore::open(&path)?;
        assert_eq!(content_offsets(&store)?, vec![limit, 2 * limit, neighbor]);
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(0)))?
                .is_none()
        );
        assert_content_index(&store)?;
        let span = (
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(span_end),
        );
        store.drop_presence_in_ranges(&[span])?;
        assert_eq!(content_offsets(&store)?, vec![neighbor]);
        assert!(
            store
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(limit)))?
                .is_none()
        );
        assert_content_index(&store)?;

        let retained = temp_dir("submodule_rocks_subspan_crash_retain");
        let path = retained.path().join("index");
        {
            let store = RocksSubmoduleStore::open(&path)?;
            seed_three_subspans(&store)?;
            put_checksums(&store, &[1])?;
            store.retain_entropy_crc_rows_one(&[first])?;
        }
        let store = RocksSubmoduleStore::open(&path)?;
        assert_eq!(content_offsets(&store)?, vec![limit, 2 * limit, neighbor]);
        let checksum = store
            .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(1)))?
            .expect("checksum in the cleared prefix");
        assert!(checksum.proof.is_none() && checksum.crc.is_some());
        assert_content_index(&store)?;
        store.retain_entropy_crc_rows(&[span])?;
        assert_eq!(content_offsets(&store)?, vec![neighbor]);
        let checksum = store
            .view(|tx| tx.get_offset_meta(PartitionChunkOffset::from(1)))?
            .expect("checksum survives entropy retain");
        assert!(checksum.proof.is_none() && checksum.crc.is_some());
        assert_content_index(&store)?;
        Ok(())
    }
}
