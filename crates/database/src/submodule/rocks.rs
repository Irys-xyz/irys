//! RocksDB submodule index.
//!
//! One plain database, one column family per table, HDD-oriented options.
//! `update` holds a mutex for the whole closure and applies a `WriteBatch`
//! only when the closure returns `Ok`. Reads inside that closure consult an
//! overlay first, so the batch sees its own uncommitted puts and deletes.
//! `view` takes a snapshot and does not take the write mutex.
//!
//! Gap scans walk tx-path intervals. A covered offset with no data-path row
//! is indexed. RocksDB has no O(1) count once the overlay hides keys, so a
//! heal scan reads the intervals that touch the window.
//!
//! Data-path and tx-path values stay in SST blocks. LZ4 compresses
//! neighbouring proofs inside one block. The `blobs` preset still extracts
//! values at or above [`BLOB_MIN_BYTES`] into blob files. A directory that
//! already has blob files keeps reading them. A compaction that rewrites
//! the SST stores those values inline, including the bottommost compaction
//! a normal open schedules once no snapshot still needs the old sequence
//! numbers. A trivial move does not rewrite the SST, so that blob file
//! stays until a later compaction reads it. rust-rocksdb 0.24 has no
//! `set_allow_fallocate` (the RocksDB default stays on). WAL recycle,
//! `wal_bytes_per_sync`,
//! `use_fsync`, and `max_total_wal_size` are the durability knobs this
//! binding exposes. Every open keeps SST readers (`max_open_files=-1`,
//! 16 opening threads). Legacy blob readers stay lazy.
//!
//! A production open shares one block cache, one write-buffer manager, and
//! the process Env thread pools. Inline path blocks use that cache. There
//! is no separate blob cache for new values. Legacy blob reads use the same
//! cache until compaction drops the files. A bench preset can opt out and
//! keep a private cache. `max_open_files` stays `-1`: a finite cap drops
//! table readers and reloads the filter on the next read.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
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
use reth_db::table::{Compress, Decode, Decompress, Encode, Table};
use rocksdb::{
    BlockBasedIndexType, BlockBasedOptions, DB, DBCompactionStyle, DBCompressionType, Direction,
    IteratorMode, Options, ReadOptions, WriteBatch, WriteOptions,
};

use super::group::{EnableStep, EngineLifetime, GroupCommit};
use super::interval::{
    IntervalEdit, IntervalRow, combine_hashes, coverage_gaps, dropped_tx_hashes, plan_coverage,
};
use super::store::PresenceReconcile;
use super::tables::{
    ChunkDataPathByOffset, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfo, DataRootInfos,
    DataRootInfosByDataRoot, PendingBodyMigration, PendingBodyMigrationsByOffset, SUBMODULE_SCHEMA,
    TxLeafBinding, TxLeafBindingByTxPathHash, TxPathByTxPathHash, TxPathInterval,
    TxPathIntervalByStart,
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
/// Age cutoff for blob GC when new blob files are off.
///
/// 1.0 marks every existing blob file old, so a compaction that rewrites
/// the SST stores each value inline. A trivial move does not read the
/// value, so that file stays until a later compaction rewrites it.
/// Bottommost compaction is one of those rewrites: a normal open runs it
/// for a file whose sequence numbers no snapshot still needs.
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
/// Live WAL cap for the rotate presets. The bench index writes about
/// 10 KiB/s of WAL, so 1 MiB switches logs about every 100 s and a 600 s
/// step runs mostly on reused WAL files. Each switch flushes every family
/// that holds data in the old log.
const WAL_ROTATE_BYTES: u64 = 1024 * 1024;
/// Live SST files on one column family that start a whole-range compact
/// in `fdatasync-wal-rotate-compact`. Baseline keeps `0` (the hook off).
const SMALL_FILE_COMPACT_THRESHOLD: usize = 64;
/// How often the compact thread rechecks live SST counts.
const SMALL_FILE_COMPACT_POLL: Duration = Duration::from_millis(100);
/// Levels counted by `rocksdb.num-files-at-level<N>` (RocksDB default).
const SST_LEVELS: usize = 7;
const SCHEMA_FILE: &str = "SCHEMA";
const SCHEMA_TEXT: &str = SUBMODULE_SCHEMA;
/// Marker written by the per-offset index. The on-disk worst-case directory
/// still has this text. A normal open refuses it.
const SCHEMA_V1_TEXT: &str = "irys-submodule-rocks 1\n";
const LEGACY_PATH_HASH_CF: &str = "ChunkPathHashesByOffset";

/// One SST in the v1 path-hash family. Keys are partition offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacySstSpan {
    pub name: String,
    pub level: i32,
    pub bytes: u64,
    pub entries: u64,
    pub start: Option<u32>,
    pub end: Option<u32>,
}

fn u32_user_key(bytes: Option<&[u8]>) -> Option<u32> {
    let bytes = bytes?;
    let raw: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(raw))
}

/// One RocksDB open. Production uses [`RocksTuning::baseline`].
///
/// The bench names the other values. Each of those changes one field, so a
/// sweep can attribute a difference to that field. Three presets change more
/// than one field: `full-filters` (one shard and a 128 MiB cache),
/// `fdatasync-wal-rotate` (`fdatasync` plus the WAL cap), and
/// `fdatasync-wal-rotate-compact` (that rotate pair plus a 64-file compact).
/// `share_resources` is not one of those fields. `cache-1g` and
/// `full-filters` turn it off so the private cache size is the one that
/// opens. `no-wal-recycle` turns WAL file reuse off (baseline reuses 4).
/// `fdatasync-wal` keeps every other baseline field and selects `fdatasync`.
/// The rotate presets cap the live WAL at [`WAL_ROTATE_BYTES`]. `all-blooms`
/// puts the 10-bit bloom on every column family. Baseline puts it only on
/// families whose point lookups miss. Options are fixed at open. A new block
/// size or a switch between partitioned and full filters needs an empty
/// directory for a fair read. An existing SST keeps the filter it was built
/// with until a compaction rewrites it. Opening that directory with the other
/// bloom scope is safe: a family with no filter policy skips a stored filter,
/// and a family whose policy name matches uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RocksTuning {
    pub name: &'static str,
    pub block_bytes: usize,
    pub block_cache_bytes: usize,
    /// `2^bits` block-cache shards. `-1` lets RocksDB choose, and that choice
    /// caps at 6 (64 shards). `0` is one shard.
    ///
    /// A filter block has to fit in one shard. The default 64 MiB cache is
    /// 1 MiB per shard, so a full filter of tens of millions of keys is
    /// inserted and dropped. `full-filters` uses one shard and 128 MiB so
    /// that block can stay.
    pub cache_shard_bits: i32,
    /// `true` is LZ4 on SST blocks and, when blob files are on, on blob files.
    /// `false` stores both uncompressed.
    pub compress: bool,
    /// Blob files for new data-path and tx-path values.
    ///
    /// `false` (baseline) stores those values in SST blocks. A family that
    /// already has blob files still reads them, and compaction inlines them.
    /// `true` (`blobs`) writes values at or above [`BLOB_MIN_BYTES`] to blob
    /// files.
    pub blobs: bool,
    /// Direct I/O for flush and compaction. Reads stay buffered.
    pub direct_io: bool,
    pub background_jobs: i32,
    pub target_file_bytes: u64,
    /// Universal compaction. `false` is leveled compaction.
    pub universal: bool,
    /// Partitioned full filters and a two-level index. The top index and
    /// filter stay pinned. `false` is one full filter per SST.
    ///
    /// Production open is `true`. A full filter for tens of millions of keys
    /// is larger than one shard of the 64 MiB block cache. Once that SST
    /// leaves level 0, every lookup reads the filter, checksums it, and
    /// drops it.
    ///
    /// A planned 4 TB partition is about 15-17 million chunks. Data-path
    /// keys stay one per chunk, so a full filter is still about 20 MB and
    /// can miss a shard. Interval rows are fewer. This open is provisional
    /// until a measurement at that size shows a full filter stays cached.
    pub partition_filters: bool,
    /// Retired WAL files to overwrite instead of allocating a new file.
    ///
    /// The baseline (production open) is `4`: that many retired files are kept
    /// and overwritten, so a later sync writes inside a file that already has
    /// blocks. `0` (`no-wal-recycle`): each WAL switch creates a file, and each
    /// sync of that growing file updates its size.
    ///
    /// RocksDB 10.4 keeps a non-zero count for `kPointInTimeRecovery` (this
    /// open's default; the binding does not set a recovery mode) and for
    /// `kSkipAnyCorruptedRecords`. Open sets the count back to 0 for
    /// `kTolerateCorruptedTailRecords` and `kAbsoluteConsistency`. A write
    /// with `disableWAL` is rejected while the count is non-zero. This store
    /// does not use `disableWAL`, `WAL_ttl_seconds`, or `WAL_size_limit_MB`.
    /// Those last two also force the count to 0.
    pub recycle_log_file_num: usize,
    /// `true` syncs the WAL, the MANIFEST, SST files, and blob files with
    /// `fsync`. `false` uses `fdatasync`.
    ///
    /// RocksDB treats the two calls as equally durable. The flag exists for
    /// old filesystem bugs (fdatasync on ext4 before Linux 3.7). It is not a
    /// separate mode for directory entries. A new file still syncs its
    /// directory through `FsyncWithDirOptions`, and that path ignores this
    /// flag.
    ///
    /// Baseline (production) is `true`. `fdatasync-wal` is `false`.
    pub use_fsync: bool,
    /// Live WAL bytes that force a memtable switch and a flush.
    ///
    /// `0` (baseline) lets RocksDB pick
    /// `4 * sum(write_buffer_size * max_write_buffer_number)`. That product
    /// is about 4 GiB with these buffers, so a bench step never fills a log.
    /// A non-zero value switches once the live logs pass it. RocksDB applies
    /// the value only when the database has more than one column family.
    /// This store has the default family plus eight.
    ///
    /// The switch flushes every column family that still references the
    /// oldest log. Each non-empty family writes an L0 SST, and compaction
    /// reads those files again. `recycle_log_file_num` can then reuse the
    /// retired log. A reused file keeps its old size, so later syncs overwrite
    /// bytes that are already allocated.
    pub max_total_wal_size: u64,
    /// Live SST files on one column family that start a whole-range compact.
    ///
    /// `0` (baseline) leaves the hook off. A positive value runs
    /// `compact_range_cf` with `BottommostLevelCompaction::Force` on that
    /// family so small files at the bottom level merge. The call runs on a
    /// dedicated thread, off the write lock, and at most one compact is in
    /// flight per database.
    pub small_file_compact_threshold: usize,
    /// `false` (baseline): 10-bit bloom only on families whose point lookups
    /// can miss. `true` (`all-blooms`): the same bloom on every family.
    ///
    /// Does not change `partition_filters`. An existing SST keeps its own
    /// filter until compaction rewrites the file.
    pub bloom_all_families: bool,
    /// Use the process-wide block cache, write-buffer manager, and Env pools.
    ///
    /// `false` builds a private cache of `block_cache_bytes` and does not
    /// attach the write-buffer manager or a rate limiter. `cache_shard_bits`
    /// applies to that private cache. The process cache uses shard bits `-1`.
    /// `cache-1g`, `full-filters`, and [`Self::with_block_cache`] set `false`
    /// so a bench can compare cache sizes. Preset diffs do not count this flag.
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
            share_resources: true,
        }
    }

    /// Named opens. `baseline` matches [`Self::baseline`].
    pub const fn presets() -> [Self; 15] {
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
            // Blob files for new path values. Baseline stores them in the SST.
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
            // WAL file reuse off, for an A/B against the baseline's 4.
            // Baseline keeps four retired files: RocksDB 10.4 does not scale
            // the count with `max_write_buffer_number` (2 here) or the column
            // family count, and one WAL covers every family. Recovery stays
            // `kPointInTimeRecovery`, which keeps a non-zero count.
            Self {
                name: "no-wal-recycle",
                recycle_log_file_num: 0,
                ..base
            },
            // `fdatasync` for the WAL, the MANIFEST, SST files, and blob files.
            // Baseline stays on `fsync`. See `RocksTuning::use_fsync`.
            Self {
                name: "fdatasync-wal",
                use_fsync: false,
                ..base
            },
            // Same `fdatasync`, plus a 1 MiB live-WAL cap. A full log
            // switches, the flush retires it, and the next log can reuse
            // that file. See `RocksTuning::max_total_wal_size`.
            Self {
                name: "fdatasync-wal-rotate",
                use_fsync: false,
                max_total_wal_size: WAL_ROTATE_BYTES,
                ..base
            },
            // Same 1 MiB cap with baseline `fsync`. A run can separate the
            // sync call from the rotation.
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
            // Same partitioned filter as baseline, on every column family.
            // Baseline leaves offset families that never miss without one.
            Self {
                name: "all-blooms",
                bloom_all_families: true,
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

/// One block cache and one write-buffer manager for every submodule index
/// that opts in. The Env is the process default. Its flush and compaction
/// pools are raised to these limits and are never reduced.
///
/// The write-buffer manager charges memtable bytes to the block cache and
/// does not stall writers. A stall waits for every writer that shares the
/// manager. The idle arena is about 1 MiB per column family and a flush
/// replaces it with another arena, so a stall cannot reclaim that floor.
/// Forty databases and nine families are about 360 MiB. That fits in the
/// 512 MiB budget. Above that floor, raise both this budget and the cache.
///
/// A path value sits in the memtable until flush whether or not blob files
/// are enabled, so this floor does not change when values stay inline.
/// After flush those bytes are SST blocks in the block cache, not a second
/// blob cache. Legacy blob reads use the same cache until compaction
/// inlines them.
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

const CF_COUNT: usize = 8;

#[derive(Clone, Copy)]
enum Cf {
    PathHashes,
    DataPath,
    TxPath,
    DataRoots,
    TxLeaf,
    Pending,
    Interval,
    Metadata,
}

impl Cf {
    const ALL: [Self; CF_COUNT] = [
        Self::PathHashes,
        Self::DataPath,
        Self::TxPath,
        Self::DataRoots,
        Self::TxLeaf,
        Self::Pending,
        Self::Interval,
        Self::Metadata,
    ];

    /// Index families. `clear` deletes these and leaves the schema row in place.
    const INDEX: [Self; 7] = [
        Self::PathHashes,
        Self::DataPath,
        Self::TxPath,
        Self::DataRoots,
        Self::TxLeaf,
        Self::Pending,
        Self::Interval,
    ];

    fn index(self) -> usize {
        match self {
            Self::PathHashes => 0,
            Self::DataPath => 1,
            Self::TxPath => 2,
            Self::DataRoots => 3,
            Self::TxLeaf => 4,
            Self::Pending => 5,
            Self::Interval => 6,
            Self::Metadata => 7,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::PathHashes => <ChunkPathHashesByOffset as Table>::NAME,
            Self::DataPath => <ChunkDataPathByOffset as Table>::NAME,
            Self::TxPath => <TxPathByTxPathHash as Table>::NAME,
            Self::DataRoots => <DataRootInfosByDataRoot as Table>::NAME,
            Self::TxLeaf => <TxLeafBindingByTxPathHash as Table>::NAME,
            Self::Pending => <PendingBodyMigrationsByOffset as Table>::NAME,
            Self::Interval => <TxPathIntervalByStart as Table>::NAME,
            Self::Metadata => <MetadataTable as Table>::NAME,
        }
    }

    /// Path families. Baseline stores new values inline and garbage-collects
    /// an existing blob file. The `blobs` preset still writes one.
    fn uses_blobs(self) -> bool {
        matches!(self, Self::DataPath | Self::TxPath)
    }

    /// Point lookups on this family can miss, so a bloom can reject a block.
    ///
    /// `bloom_all_families` forces the filter on the other families too.
    fn uses_bloom(self, tuning: &RocksTuning) -> bool {
        tuning.bloom_all_families
            || matches!(
                self,
                Self::PathHashes | Self::TxPath | Self::DataRoots | Self::TxLeaf
            )
    }
}

/// Count-triggered whole-range compact. One thread per database.
///
/// The thread waits on [`SMALL_FILE_COMPACT_POLL`]. `compact_range_cf` then
/// runs on RocksDB's background pool and this thread blocks until it
/// finishes. The write mutex stays free. `threshold == 0` skips the thread.
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
    for cf in Cf::ALL {
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

#[derive(Clone)]
pub struct RocksSubmoduleStore {
    /// Dropped first so the delay thread joins before `db` closes.
    lifetime: Arc<EngineLifetime>,
    /// Joins next so a CompactRange finishes before `db` closes.
    compact: Arc<SmallFileCompactor>,
    db: Arc<DB>,
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
        Self::open_inner(path, RocksTuning::baseline(), true, false, None, true)
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
        Self::open_inner(path, tuning, false, false, None, true)
    }

    /// Bench open. Same table cache as [`Self::open`], and it skips the
    /// table-property scan during `DB::Open`.
    ///
    /// Histograms and timers stay off.
    pub fn open_with_stats(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Self::open_inner(path, tuning, true, true, None, true)
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
        )
    }

    /// `true` when this database uses a shared cache and write-buffer manager.
    pub fn uses_shared_resources(&self) -> bool {
        self.resources.is_some()
    }

    /// Opens a conversion target. Does not write the SCHEMA file or the schema row.
    ///
    /// Uses [`RocksTuning::baseline`], so copied path values are inline.
    /// [`Self::stamp_schema_marker`] writes both markers after the copy is
    /// durable. A crash before that leaves `CURRENT` and no SCHEMA file.
    /// [`Self::open`] refuses that directory.
    pub(super) fn open_converting(path: impl AsRef<Path>) -> eyre::Result<Self> {
        Self::open_inner(path, RocksTuning::baseline(), true, false, None, false)
    }

    /// Open a schema-v1 directory for point reads of `ChunkPathHashesByOffset`.
    ///
    /// The column families already on disk are opened. Missing v2 families are
    /// not created, and the schema marker is not rewritten. `tuning` applies
    /// to this open. An existing SST keeps the filter it was flushed with
    /// until [`Self::compact_legacy_path_hashes`].
    pub fn open_v1_probe(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        check_schema_text(&path, SCHEMA_V1_TEXT)?;
        let resources = shared_resources_for(tuning, None)?;
        let (db, stats) = open_existing_db(&path, &tuning, resources.as_deref())?;
        let group = GroupCommit::new();
        let db = Arc::new(db);
        let submodule = submodule_label(&path);
        Ok(Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            compact: SmallFileCompactor::start(Arc::clone(&db), 0, submodule.clone())?,
            db,
            resources,
            write: Arc::new(Mutex::new(())),
            group,
            submodule,
            path,
            stats,
        })
    }

    /// True when this offset has a path-hash row in the v1 column family.
    pub fn legacy_path_hash_present(&self, offset: PartitionChunkOffset) -> eyre::Result<bool> {
        let handle = self.legacy_path_hash_cf()?;
        Ok(self
            .db
            .get_cf(&handle, encode_key(offset))
            .wrap_err("read legacy path-hash row")?
            .is_some())
    }

    /// Rewrite `ChunkPathHashesByOffset` with the table options of this open.
    ///
    /// A plain `compact_range` trivial-moves one SST and leaves its filter
    /// in place. Forcing the bottom level rewrites that file. The schema
    /// marker stays v1.
    pub fn compact_legacy_path_hashes(&self) -> eyre::Result<()> {
        let _guard = self.lock_write();
        let handle = self.legacy_path_hash_cf()?;
        let mut opts = rocksdb::CompactOptions::default();
        opts.set_bottommost_level_compaction(rocksdb::BottommostLevelCompaction::Force);
        opts.set_exclusive_manual_compaction(true);
        self.db
            .compact_range_cf_opt(&handle, None::<&[u8]>, None::<&[u8]>, &opts);
        Ok(())
    }

    /// Live SST files in the v1 path-hash family, oldest level first.
    pub fn legacy_path_hash_ssts(&self) -> eyre::Result<Vec<LegacySstSpan>> {
        let mut files = Vec::new();
        for file in self.db.live_files().wrap_err("list live sst files")? {
            if file.column_family_name != LEGACY_PATH_HASH_CF {
                continue;
            }
            files.push(LegacySstSpan {
                name: file
                    .name
                    .rsplit('/')
                    .next()
                    .unwrap_or(file.name.as_str())
                    .to_string(),
                level: file.level,
                bytes: u64::try_from(file.size).unwrap_or(u64::MAX),
                entries: file.num_entries,
                start: u32_user_key(file.start_key.as_deref()),
                end: u32_user_key(file.end_key.as_deref()),
            });
        }
        files.sort_by(|left, right| {
            left.level
                .cmp(&right.level)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(files)
    }

    fn legacy_path_hash_cf(&self) -> eyre::Result<&rocksdb::ColumnFamily> {
        self.db
            .cf_handle(LEGACY_PATH_HASH_CF)
            .ok_or_else(|| eyre::eyre!("missing column family {LEGACY_PATH_HASH_CF}"))
    }

    fn open_inner(
        path: impl AsRef<Path>,
        tuning: RocksTuning,
        collect_stats: bool,
        preload_tables: bool,
        resources: Option<Arc<RocksResources>>,
        stamp_schema: bool,
    ) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if stamp_schema {
            // A directory with CURRENT and no SCHEMA is a crashed conversion
            // or a crashed first open. Stamping it here would hide missing rows.
            refuse_incomplete_directory(&path)?;
            check_schema_file(&path)?;
        } else if path.join(SCHEMA_FILE).is_file() {
            eyre::bail!(
                "refusing to convert into completed rocks index {}",
                path.display()
            );
        }
        let resources = shared_resources_for(tuning, resources)?;
        let (db, stats) = open_db(
            &path,
            &tuning,
            collect_stats,
            preload_tables,
            resources.as_deref(),
        )?;
        if stamp_schema {
            ensure_schema_row(&db)?;
            let marker = path.join(SCHEMA_FILE);
            if !marker.exists() {
                fs::write(&marker, SCHEMA_TEXT).wrap_err("write schema marker")?;
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
            resources,
            write: Arc::new(Mutex::new(())),
            group,
            submodule,
            path,
            stats,
        })
    }

    /// Syncs the copied rows, writes the schema row, then the SCHEMA file.
    ///
    /// The file is last. A crash before the rename leaves no completion marker.
    pub(super) fn stamp_schema_marker(&self) -> eyre::Result<()> {
        let _guard = self.lock_write();
        self.db
            .flush_wal(true)
            .wrap_err("sync wal before schema marker")?;
        ensure_schema_row(&self.db)?;
        write_schema_marker_file(&self.path)
    }

    pub(super) fn schema_bytes(&self) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let key = encode_key(MetadataKey::DBSchemaVersion);
        match self.db.get_cf(&handle, key).wrap_err("read schema row")? {
            Some(bytes) => Ok(Some(Vec::<u8>::decompress(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Next rows after `after`, in key order. A single oversized value is
    /// still returned so the caller moves forward.
    pub(super) fn read_cf_batch<K, V, F>(
        &self,
        cf: &str,
        after: Option<K>,
        limit_rows: usize,
        limit_bytes: usize,
        value_len: F,
    ) -> eyre::Result<Vec<(K, V)>>
    where
        K: Decode + Encode + Ord + Copy,
        V: Decompress,
        F: Fn(&V) -> usize,
    {
        let limit_rows = limit_rows.max(1);
        let limit_bytes = limit_bytes.max(1);
        let handle = self
            .db
            .cf_handle(cf)
            .ok_or_else(|| eyre::eyre!("missing column family {cf}"))?;
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        let encoded = after.map(encode_key);
        let mode = match &encoded {
            None => IteratorMode::Start,
            Some(bytes) => IteratorMode::From(bytes.as_slice(), Direction::Forward),
        };
        let snap = self.db.snapshot();
        let mut out = Vec::new();
        let mut bytes_used = 0_usize;
        for item in snap.iterator_cf_opt(handle, opts, mode) {
            let (key, value) = item.wrap_err("scan rocksdb row")?;
            let key =
                K::decode(key.as_ref()).map_err(|err| eyre::eyre!("decode key in {cf}: {err}"))?;
            if after.is_some_and(|prev| key <= prev) {
                continue;
            }
            let value = V::decompress(value.as_ref())
                .map_err(|err| eyre::eyre!("decode value in {cf}: {err}"))?;
            let next_bytes = bytes_used.saturating_add(value_len(&value));
            if !out.is_empty() && (out.len() >= limit_rows || next_bytes > limit_bytes) {
                break;
            }
            bytes_used = next_bytes;
            out.push((key, value));
            if out.len() >= limit_rows || bytes_used >= limit_bytes {
                break;
            }
        }
        Ok(out)
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
        };
        let result = f(&mut batch)?;
        batch.commit(sync, &self.submodule)?;
        if sync {
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
        set_db_gauge(
            &self.db,
            rocksdb::properties::IS_WRITE_STOPPED,
            "irys.rocksdb.write_stopped",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::ACTUAL_DELAYED_WRITE_RATE,
            "irys.rocksdb.delayed_write_rate",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::NUM_RUNNING_COMPACTIONS,
            "irys.rocksdb.running_compactions",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::NUM_RUNNING_FLUSHES,
            "irys.rocksdb.running_flushes",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::COMPACTION_PENDING,
            "irys.rocksdb.compaction_pending",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::BLOCK_CACHE_USAGE,
            "irys.rocksdb.block_cache_usage_bytes",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::BLOCK_CACHE_CAPACITY,
            "irys.rocksdb.block_cache_capacity_bytes",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::BLOCK_CACHE_PINNED_USAGE,
            "irys.rocksdb.block_cache_pinned_bytes",
            submodule,
        );
        set_db_gauge(
            &self.db,
            rocksdb::properties::CUR_SIZE_ALL_MEM_TABLES,
            "irys.rocksdb.memtable_bytes",
            submodule,
        );
        for cf in Cf::ALL {
            let Ok(handle) = cf_handle(&self.db, cf) else {
                continue;
            };
            let name = cf.name();
            set_cf_gauge(
                &self.db,
                handle,
                rocksdb::properties::ESTIMATE_NUM_KEYS,
                "irys.rocksdb.estimate_num_keys",
                submodule,
                name,
            );
            set_cf_gauge(
                &self.db,
                handle,
                rocksdb::properties::LIVE_SST_FILES_SIZE,
                "irys.rocksdb.live_sst_files_size_bytes",
                submodule,
                name,
            );
            set_cf_gauge(
                &self.db,
                handle,
                rocksdb::properties::SIZE_ALL_MEM_TABLES,
                "irys.rocksdb.memtable_size_bytes",
                submodule,
                name,
            );
            set_cf_gauge(
                &self.db,
                handle,
                rocksdb::properties::ESTIMATE_PENDING_COMPACTION_BYTES,
                "irys.rocksdb.pending_compaction_bytes",
                submodule,
                name,
            );
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
        metrics::counter!("irys.rocksdb.wal_syncs", "submodule" => submodule.to_owned())
            .absolute(self.group.wal_syncs());
        metrics::counter!(
            "irys.rocksdb.wal_backstop_syncs",
            "submodule" => submodule.to_owned()
        )
        .absolute(self.group.backstop_syncs());
        let (compactions, files_before, files_after) = self.compact.state.snapshot();
        metrics::counter!(
            "irys.rocksdb.small_file_compactions",
            "submodule" => submodule.to_owned()
        )
        .absolute(compactions);
        metrics::counter!(
            "irys.rocksdb.small_file_compact_files_before",
            "submodule" => submodule.to_owned()
        )
        .absolute(files_before);
        metrics::counter!(
            "irys.rocksdb.small_file_compact_files_after",
            "submodule" => submodule.to_owned()
        )
        .absolute(files_after);
        let Some(stats) = &self.stats else {
            return;
        };
        set_ticker(
            stats,
            BlockCacheFilterMiss,
            "irys.rocksdb.block_cache_filter_miss",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheFilterHit,
            "irys.rocksdb.block_cache_filter_hit",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheFilterAdd,
            "irys.rocksdb.block_cache_filter_add",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheFilterBytesInsert,
            "irys.rocksdb.block_cache_filter_bytes_insert",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheIndexMiss,
            "irys.rocksdb.block_cache_index_miss",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheIndexHit,
            "irys.rocksdb.block_cache_index_hit",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheDataMiss,
            "irys.rocksdb.block_cache_data_miss",
            submodule,
        );
        set_ticker(
            stats,
            BlockCacheDataHit,
            "irys.rocksdb.block_cache_data_hit",
            submodule,
        );
        set_ticker(stats, BytesRead, "irys.rocksdb.bytes_read", submodule);
        set_ticker(stats, NumberKeysRead, "irys.rocksdb.keys_read", submodule);
        set_ticker(stats, StallMicros, "irys.rocksdb.stall_micros", submodule);
        set_ticker(stats, NoFileOpens, "irys.rocksdb.no_file_opens", submodule);
        set_ticker(
            stats,
            CompactReadBytes,
            "irys.rocksdb.compact_read_bytes",
            submodule,
        );
        set_ticker(
            stats,
            CompactWriteBytes,
            "irys.rocksdb.compact_write_bytes",
            submodule,
        );
        set_ticker(
            stats,
            FlushWriteBytes,
            "irys.rocksdb.flush_write_bytes",
            submodule,
        );
        set_ticker(
            stats,
            WalFileSynced,
            "irys.rocksdb.wal_file_synced",
            submodule,
        );
        set_ticker(
            stats,
            WalFileBytes,
            "irys.rocksdb.wal_file_bytes",
            submodule,
        );
    }

    /// Flush memtables, compact every column family, and sync the WAL.
    ///
    /// Takes the write lock. Do not call it from inside `update`.
    pub fn flush_and_compact(&self) -> eyre::Result<()> {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for cf in Cf::ALL {
            let handle = cf_handle(&self.db, cf)?;
            self.db.flush_cf(&handle).wrap_err("flush column family")?;
            self.db
                .compact_range_cf(&handle, None::<&[u8]>, None::<&[u8]>);
        }
        self.db.flush_wal(true).wrap_err("sync wal")?;
        // The synced WAL covers any open registration group. This flush is
        // the caller's durable point, not a backstop sync.
        self.group.note_wal_sync();
        self.group.cover_durable();
        Ok(())
    }

    /// Cumulative write-stall time and the current compaction and flush gauges.
    ///
    /// Stall time comes from the cumulative line of `rocksdb.dbstats`. That
    /// string property also resets RocksDB's interval counters. Pending
    /// compaction bytes and flushed immutable memtables are summed across
    /// column families. Running flushes is the database-wide gauge.
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
        for cf in Cf::ALL {
            let handle = cf_handle(&self.db, cf)?;
            pending_compaction_bytes = pending_compaction_bytes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.estimate-pending-compaction-bytes",
            )?);
            flushes = flushes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.num-immutable-mem-table-flushed",
            )?);
        }
        if let Some(handle) = self.db.cf_handle("default") {
            pending_compaction_bytes = pending_compaction_bytes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.estimate-pending-compaction-bytes",
            )?);
            flushes = flushes.saturating_add(int_property_cf(
                &self.db,
                handle,
                "rocksdb.num-immutable-mem-table-flushed",
            )?);
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
    /// Counts keys only. `DeleteRange` then removes them. A span with no
    /// presence key does not enter the batch. An empty result does not write,
    /// so the sequence stays put.
    pub(super) fn delete_presence_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        if ranges.iter().all(|(start, end)| start > end) {
            return Ok(PresenceReconcile::default());
        }
        let _guard = self.lock_write();
        let snap = self.db.snapshot();
        let mut path_hash_rows = 0_u64;
        let mut data_path_rows = 0_u64;
        let mut deletes = Vec::new();
        for &(start, end) in ranges {
            if start > end {
                continue;
            }
            let path_count = count_presence_keys(&self.db, &snap, Cf::PathHashes, start, end)?;
            let data_count = count_presence_keys(&self.db, &snap, Cf::DataPath, start, end)?;
            path_hash_rows = path_hash_rows.saturating_add(path_count);
            data_path_rows = data_path_rows.saturating_add(data_count);
            if path_count > 0 {
                deletes.push((Cf::PathHashes, start, end));
            }
            if data_count > 0 {
                deletes.push((Cf::DataPath, start, end));
            }
        }
        drop(snap);
        if deletes.is_empty() {
            return Ok(PresenceReconcile {
                path_hash_rows,
                data_path_rows,
            });
        }
        let mut batch = WriteBatch::new();
        for (cf, start, end) in deletes {
            let handle = cf_handle(&self.db, cf)?;
            batch.delete_range_cf(&handle, encode_key(start), exclusive_end_key(end));
        }
        let mut opts = WriteOptions::default();
        opts.set_sync(true);
        self.db
            .write_opt(batch, &opts)
            .wrap_err("reconcile presence rows")?;
        self.group.note_wal_sync();
        self.group.cover_durable();
        Ok(PresenceReconcile {
            path_hash_rows,
            data_path_rows,
        })
    }

    #[cfg(test)]
    fn flush_memtables(&self) -> eyre::Result<()> {
        let _guard = self.lock_write();
        for cf in Cf::ALL {
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
        for cf in Cf::ALL {
            let handle = cf_handle(&self.db, cf)?;
            self.db
                .set_options_cf(handle, &[("disable_auto_compactions", "true")])
                .wrap_err("disable auto compaction")?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn path_hash_sst_count(&self) -> u64 {
        cf_handle(&self.db, Cf::PathHashes)
            .map(|handle| live_sst_count(&self.db, handle))
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn schema_payload(&self) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let key = encode_key(MetadataKey::DBSchemaVersion);
        match self.db.get_cf(&handle, key).wrap_err("read schema row")? {
            Some(bytes) => Ok(Some(Vec::<u8>::decompress(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Visible keys. Does not read values. A range tombstone hides its span.
    #[cfg(test)]
    fn cf_key_count(&self, cf: Cf) -> eyre::Result<usize> {
        let handle = cf_handle(&self.db, cf)?;
        let snap = self.db.snapshot();
        let mut opts = ReadOptions::default();
        opts.fill_cache(false);
        let mut iter = snap.raw_iterator_cf_opt(&handle, opts);
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
        self.group.cover_durable();
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
}

impl RocksSubmoduleStore {
    /// Synced metadata write. Same WAL barrier as `update`. Not part of `clear`.
    fn write_metadata(&self, key: MetadataKey, value: Option<Vec<u8>>) -> eyre::Result<()> {
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let mut batch = WriteBatch::new();
        let encoded = encode_key(key);
        match &value {
            Some(bytes) => {
                batch.put_cf(&handle, encoded, compress_value(bytes));
            }
            None => batch.delete_cf(&handle, encoded),
        }
        let mut opts = WriteOptions::default();
        opts.set_sync(true);
        self.db
            .write_opt(batch, &opts)
            .wrap_err("write submodule metadata")?;
        self.group.note_wal_sync();
        self.group.cover_durable();
        Ok(())
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
        metrics::describe_gauge!(
            "irys.rocksdb.estimate_num_keys",
            "Estimated live keys in one submodule column family"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.live_sst_files_size_bytes",
            "Live SST bytes in one submodule column family"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.memtable_size_bytes",
            "Memtable bytes in one submodule column family"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.pending_compaction_bytes",
            "Estimated bytes waiting for compaction in one submodule column family"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.files_at_level0",
            "Level-0 files in one submodule column family"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.write_stopped",
            "1 when RocksDB has stopped writes on this submodule database"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.delayed_write_rate",
            "Current delayed write rate for this submodule database, bytes per second"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.running_compactions",
            "Compactions running on this submodule database"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.running_flushes",
            "Flushes running on this submodule database"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.compaction_pending",
            "1 when a compaction is pending on this submodule database"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.block_cache_usage_bytes",
            "Bytes in the block cache this submodule database uses. A shared cache repeats this total on every submodule. Do not add those series."
        );
        metrics::describe_gauge!(
            "irys.rocksdb.block_cache_capacity_bytes",
            "Block cache capacity for this submodule database. A shared cache repeats this capacity on every submodule."
        );
        metrics::describe_gauge!(
            "irys.rocksdb.block_cache_pinned_bytes",
            "Pinned bytes in the block cache this submodule database uses. A shared cache repeats this total on every submodule."
        );
        metrics::describe_gauge!(
            "irys.rocksdb.memtable_bytes",
            "Memtable bytes in this submodule database, summed across column families"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.shared_block_cache_usage_bytes",
            "Bytes in the process-wide RocksDB block cache, including the write-buffer charge"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.shared_block_cache_pinned_bytes",
            "Pinned bytes in the process-wide RocksDB block cache"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.shared_block_cache_capacity_bytes",
            "Capacity of the process-wide RocksDB block cache"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.shared_write_buffer_usage_bytes",
            "Memtable bytes charged to the process-wide write buffer manager"
        );
        metrics::describe_gauge!(
            "irys.rocksdb.shared_write_buffer_capacity_bytes",
            "Budget of the process-wide write buffer manager"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_filter_miss",
            "Block-cache filter misses since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_filter_hit",
            "Block-cache filter hits since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_filter_add",
            "Block-cache filter inserts since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_filter_bytes_insert",
            "Block-cache filter bytes inserted since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_index_miss",
            "Block-cache index misses since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_index_hit",
            "Block-cache index hits since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_data_miss",
            "Block-cache data misses since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.block_cache_data_hit",
            "Block-cache data hits since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.bytes_read",
            "Bytes read by Gets since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.keys_read",
            "Keys read since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.stall_micros",
            "Microseconds spent in write stalls since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.no_file_opens",
            "Table and blob file opens since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.compact_read_bytes",
            "Bytes read by compaction since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.compact_write_bytes",
            "Bytes written by compaction since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.flush_write_bytes",
            "Bytes written by flush since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.wal_file_synced",
            "WAL syncs since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.wal_syncs",
            "Synced WAL writes on this submodule database, including backstop syncs"
        );
        metrics::describe_counter!(
            "irys.rocksdb.wal_backstop_syncs",
            "WAL syncs from the backstop thread, an explicit group sync, or shutdown"
        );
        metrics::describe_counter!(
            "irys.rocksdb.wal_file_bytes",
            "WAL bytes written since this submodule database opened"
        );
        metrics::describe_counter!(
            "irys.rocksdb.small_file_compactions",
            "Count-triggered compact_range calls on this submodule database"
        );
        metrics::describe_counter!(
            "irys.rocksdb.small_file_compact_files_before",
            "Live SST files counted before each count-triggered compact_range"
        );
        metrics::describe_counter!(
            "irys.rocksdb.small_file_compact_files_after",
            "Live SST files counted after each count-triggered compact_range"
        );
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

fn open_db(
    path: &Path,
    tuning: &RocksTuning,
    collect_stats: bool,
    preload_tables: bool,
    resources: Option<&RocksResources>,
) -> eyre::Result<(DB, Option<Arc<Options>>)> {
    let cache = open_cache(tuning, resources);
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    // Per-database cap so one index cannot take every shared pool thread.
    // The pools themselves are process-wide. This value stays at or below
    // those pools for the baseline (2 jobs asks for 1 flush and 1
    // compaction). A higher `background_jobs` still grows the pool:
    // RocksDB only increases it.
    db_opts.set_max_background_jobs(tuning.background_jobs);
    db_opts.set_bytes_per_sync(1024 * 1024);
    db_opts.set_wal_bytes_per_sync(1024 * 1024);
    // Baseline (production) is 4; `no-wal-recycle` is 0.
    // Point-in-time recovery, the default, keeps a non-zero count.
    // See `RocksTuning::recycle_log_file_num`.
    db_opts.set_recycle_log_file_num(tuning.recycle_log_file_num);
    // Baseline (production) is fsync. A bench preset can select fdatasync.
    // The flag also covers the MANIFEST, SST files, and blob files.
    // See `RocksTuning::use_fsync`.
    db_opts.set_use_fsync(tuning.use_fsync);
    // `0` keeps the RocksDB dynamic cap. A rotate preset sets 1 MiB.
    // See `RocksTuning::max_total_wal_size`.
    db_opts.set_max_total_wal_size(tuning.max_total_wal_size);
    // A 512 cap evicts SST readers. The next Get then reads the footer,
    // index, and filter. -1 keeps every table open. Legacy blob readers
    // stay lazy. A full submodule is a few dozen SST files. A directory
    // that still has blob files adds those, and they go away as compaction
    // inlines them.
    // Forty of those stay under a normal nofile limit. A finite cap brings
    // the table-reader reload cliff back, so this stays -1.
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

    let families = user_column_families(&cache, tuning);
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
) -> Vec<rocksdb::ColumnFamilyDescriptor> {
    let mut families: Vec<_> = Cf::ALL
        .into_iter()
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
    families
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
    // Full filter, 10 bits per key. `false` is not a block-based filter.
    // Only families whose point Gets can miss pay for one. A hit still
    // reads the value, and an iterator seek never consults a filter, so
    // the filter is a second I/O and a cache resident on those families.
    //
    // DataPath Gets run only after the offset row already names a data-path
    // hash. Interval reads are floor seeks and range scans. Pending point
    // Gets use a key the pending scan just returned; that scan is itself
    // an iterator. `optimize_filters_for_hits` would still build a filter
    // for every non-bottom SST and pin the L0 copy. That is the resident
    // set this skips, including while the rows are still in L0.
    //
    // PathHashes misses when no data-path row is stored yet (an indexed
    // offset whose body is absent). TxPath, TxLeaf, and DataRoots are
    // hash keys a caller may not have stored. Those four keep the bloom.
    // Partitioned filters are the production shape. See
    // `RocksTuning::partition_filters`. `all-blooms` restores the filter
    // on the other families.
    let bloom = cf.uses_bloom(tuning);
    if bloom {
        table.set_bloom_filter(10.0, false);
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
            // `enable_blob_files` stays off, so a flushed value is an SST
            // record. GC still runs: a compaction that rewrites an old blob
            // index stores the bytes inline. Cutoff 1.0 selects every blob
            // file. A trivial move does not rewrite, so that file stays
            // until a later compaction reads it.
            opts.set_enable_blob_files(false);
            opts.set_enable_blob_gc(true);
            opts.set_blob_gc_age_cutoff(LEGACY_BLOB_GC_AGE_CUTOFF);
        }
    }
    #[cfg(test)]
    if OPEN_DISABLE_AUTO_COMPACTION.with(std::cell::Cell::get) {
        // A normal open schedules bottommost compaction, which rewrites a
        // blob SST and inlines it. Tests that must read the blob file set
        // this before open. Manual compaction still runs.
        opts.set_disable_auto_compactions(true);
    }
    opts
}

#[cfg(test)]
thread_local! {
    static OPEN_DISABLE_AUTO_COMPACTION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// `CURRENT` without SCHEMA is an unfinished conversion. Open must not
/// create the marker, or the node would serve a partial index.
fn refuse_incomplete_directory(path: &Path) -> eyre::Result<()> {
    if path.join(SCHEMA_FILE).exists() {
        return Ok(());
    }
    if path.join("CURRENT").is_file() {
        eyre::bail!(
            "submodule index {} has RocksDB files but no SCHEMA marker, so the conversion is incomplete. Delete this directory and rerun `submodule-index-convert mdbx-to-rocks` with --replace-incomplete. Do not start the node on it. If a completed rocks/ index has rows written after the switch, rollback is `submodule-index-convert rocks-to-mdbx`.",
            path.display()
        );
    }
    Ok(())
}

fn check_schema_file(path: &Path) -> eyre::Result<()> {
    let marker = path.join(SCHEMA_FILE);
    if !marker.exists() {
        return Ok(());
    }
    check_schema_text(path, SCHEMA_TEXT)
}

/// Writes SCHEMA via a temp file and a rename, after the bytes are synced.
fn write_schema_marker_file(dir: &Path) -> eyre::Result<()> {
    let tmp = dir.join("SCHEMA.tmp");
    let final_path = dir.join(SCHEMA_FILE);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .wrap_err("create schema marker")?;
        file.write_all(SCHEMA_TEXT.as_bytes())
            .wrap_err("write schema marker")?;
        file.sync_all().wrap_err("sync schema marker")?;
    }
    fs::rename(&tmp, &final_path).wrap_err("rename schema marker")?;
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

fn check_schema_text(path: &Path, expected: &str) -> eyre::Result<()> {
    let marker = path.join(SCHEMA_FILE);
    let text = fs::read_to_string(&marker).wrap_err("read schema marker")?;
    if text != expected {
        eyre::bail!(
            "schema file {} does not match this index engine",
            marker.display()
        );
    }
    Ok(())
}

/// Table options for a column family name already stored in a v1 directory.
fn legacy_cf_kind(name: &str) -> Cf {
    match name {
        LEGACY_PATH_HASH_CF => Cf::PathHashes,
        "ChunkDataPathByPathHash" | "ChunkDataPathByOffset" => Cf::DataPath,
        "TxPathByTxPathHash" => Cf::TxPath,
        "DataRootInfosByDataRoot" => Cf::DataRoots,
        "TxLeafBindingByTxPathHash" => Cf::TxLeaf,
        "PendingBodyMigrationsByOffset" => Cf::Pending,
        "TxPathIntervalByStart" => Cf::Interval,
        _ => Cf::Metadata,
    }
}

fn open_existing_db(
    path: &Path,
    tuning: &RocksTuning,
    resources: Option<&RocksResources>,
) -> eyre::Result<(DB, Option<Arc<Options>>)> {
    let cache = open_cache(tuning, resources);
    let mut db_opts = Options::default();
    db_opts.create_if_missing(false);
    db_opts.create_missing_column_families(false);
    db_opts.set_max_background_jobs(tuning.background_jobs);
    db_opts.set_bytes_per_sync(1024 * 1024);
    db_opts.set_wal_bytes_per_sync(1024 * 1024);
    db_opts.set_recycle_log_file_num(tuning.recycle_log_file_num);
    db_opts.set_use_fsync(tuning.use_fsync);
    db_opts.set_max_total_wal_size(tuning.max_total_wal_size);
    db_opts.set_max_open_files(-1);
    db_opts.set_max_file_opening_threads(16);
    db_opts.set_skip_stats_update_on_db_open(true);
    db_opts.enable_statistics();
    db_opts.set_statistics_level(rocksdb::statistics::StatsLevel::ExceptHistogramOrTimers);
    db_opts.set_stats_dump_period_sec(0);
    db_opts.set_stats_persist_period_sec(0);
    if let Some(resources) = resources {
        apply_shared_limits(&mut db_opts, resources)?;
    }

    let names = DB::list_cf(&Options::default(), path).wrap_err("list column families")?;
    eyre::ensure!(
        names.iter().any(|name| name == LEGACY_PATH_HASH_CF),
        "missing column family {LEGACY_PATH_HASH_CF}"
    );
    // Keep "default" in the list. Omitting it makes the binding open that
    // family with a private 32 MiB cache.
    let families = names.into_iter().map(|name| {
        let opts = column_options(&cache, legacy_cf_kind(&name), tuning);
        rocksdb::ColumnFamilyDescriptor::new(name, opts)
    });
    let db = DB::open_cf_descriptors(&db_opts, path, families)
        .wrap_err("open existing rocksdb submodule index")?;
    Ok((db, Some(Arc::new(db_opts))))
}

fn ensure_schema_row(db: &DB) -> eyre::Result<()> {
    let handle = cf_handle(db, Cf::Metadata)?;
    let key = encode_key(MetadataKey::DBSchemaVersion);
    let expected = schema_value();
    match db.get_cf(&handle, &key).wrap_err("read schema row")? {
        Some(found) if found == expected => Ok(()),
        Some(_) => eyre::bail!("schema row does not match this index engine"),
        None => {
            let mut batch = WriteBatch::new();
            batch.put_cf(&handle, key, expected);
            let mut opts = WriteOptions::default();
            opts.set_sync(true);
            db.write_opt(batch, &opts).wrap_err("write schema row")
        }
    }
}

fn schema_value() -> Vec<u8> {
    compress_value(&SCHEMA_TEXT.as_bytes().to_vec())
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

/// Exclusive end of every key in `cf`.
///
/// Offset families use a 4-byte big-endian key. Hash families use 32 bytes.
/// The extra `0` sorts after the all-`0xFF` key of that width, so the
/// half-open range covers the last key. `Metadata` is not cleared.
fn full_cf_end(cf: Cf) -> Vec<u8> {
    match cf {
        Cf::PathHashes | Cf::DataPath | Cf::Pending | Cf::Interval => {
            exclusive_end_key(PartitionChunkOffset(u32::MAX))
        }
        Cf::TxPath | Cf::DataRoots | Cf::TxLeaf | Cf::Metadata => {
            let mut key = vec![0xFF; 32];
            key.push(0);
            key
        }
    }
}

/// `true` when the family has a key. Does not read the value.
fn cf_has_key(db: &DB, snap: &rocksdb::Snapshot<'_>, cf: Cf) -> eyre::Result<bool> {
    let handle = cf_handle(db, cf)?;
    let mut opts = ReadOptions::default();
    opts.fill_cache(false);
    let mut iter = snap.raw_iterator_cf_opt(&handle, opts);
    iter.seek_to_first();
    let present = iter.key().is_some();
    iter.status().wrap_err("seek column family")?;
    Ok(present)
}

/// `true` when `key` is an offset inside one of `ranges`.
///
/// Hash keys are 32 bytes. They do not decode as offsets and are not hidden.
fn offset_key_in_ranges(ranges: &[(u32, u32)], key: &[u8]) -> bool {
    let Ok(offset) = PartitionChunkOffset::decode(key) else {
        return false;
    };
    ranges
        .iter()
        .any(|(start, end)| offset.0 >= *start && offset.0 <= *end)
}

/// Keys in `[start, end]`. Does not read values.
fn count_presence_keys(
    db: &DB,
    snap: &rocksdb::Snapshot<'_>,
    cf: Cf,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<u64> {
    let handle = cf_handle(db, cf)?;
    let mut opts = ReadOptions::default();
    opts.fill_cache(false);
    opts.set_iterate_upper_bound(exclusive_end_key(end));
    let mut iter = snap.raw_iterator_cf_opt(&handle, opts);
    iter.seek(encode_key(start));
    let mut count = 0_u64;
    while iter.key().is_some() {
        count = count.saturating_add(1);
        iter.next();
    }
    iter.status().wrap_err("count presence keys")?;
    Ok(count)
}

fn compress_value<V: Compress>(value: &V) -> Vec<u8> {
    let mut buf = Vec::new();
    value.compress_to_buf(&mut buf);
    buf
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
}

struct Batch<'a> {
    db: &'a DB,
    snap: &'a rocksdb::Snapshot<'a>,
    overlay: Overlay,
}

impl<'a> Batch<'a> {
    fn rows(&self) -> Rows<'_> {
        Rows {
            db: self.db,
            snap: self.snap,
            overlay: Some(&self.overlay),
        }
    }

    fn put<V: Compress>(&mut self, cf: Cf, key: &[u8], value: &V) {
        self.overlay.maps[cf.index()].insert(key.to_vec(), Some(compress_value(value)));
    }

    fn delete(&mut self, cf: Cf, key: &[u8]) {
        self.overlay.maps[cf.index()].insert(key.to_vec(), None);
    }

    fn put_data_path_hash(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<ChunkPathHash>,
    ) {
        let key = encode_key(offset);
        match path_hash {
            Some(data_path_hash) => self.put(
                Cf::PathHashes,
                &key,
                &ChunkPathHashes {
                    data_path_hash: Some(data_path_hash),
                    tx_path_hash: None,
                },
            ),
            None => self.delete(Cf::PathHashes, &key),
        }
    }

    fn assign_tx_interval(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        tx_path_hash: Option<H256>,
    ) -> eyre::Result<()> {
        if start > end {
            return Ok(());
        }
        let existing = self.rows().intervals_touching(start, end)?;
        let edit = plan_coverage(&existing, start, end, tx_path_hash);
        for key in &edit.delete {
            self.delete(Cf::Interval, &encode_key(*key));
        }
        for row in &edit.put {
            self.put(
                Cf::Interval,
                &encode_key(row.start),
                &TxPathInterval {
                    end: row.end,
                    tx_path_hash: row.tx_path_hash,
                },
            );
        }
        self.drop_unreferenced_tx_paths(&existing, &edit)
    }

    /// Delete a tx path and its leaf binding when no interval names that hash.
    ///
    /// The candidate set is the hashes that left the edit window. An empty set
    /// skips the scan. A hash that another interval still names stays.
    fn drop_unreferenced_tx_paths(
        &mut self,
        existing: &[IntervalRow],
        edit: &IntervalEdit,
    ) -> eyre::Result<()> {
        let mut dropped = dropped_tx_hashes(existing, edit);
        if dropped.is_empty() {
            return Ok(());
        }
        {
            let rows = self.rows();
            let mut cursor = rows.cursor(Cf::Interval, None)?;
            while let Some((_, value)) = cursor.next_kv()? {
                let interval = TxPathInterval::decompress(&value)?;
                dropped.retain(|hash| *hash != interval.tx_path_hash);
                if dropped.is_empty() {
                    return Ok(());
                }
            }
        }
        for hash in dropped {
            let key = encode_key(hash);
            self.delete(Cf::TxPath, &key);
            self.delete(Cf::TxLeaf, &key);
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
        let index = cf.index();
        self.overlay.maps[index].retain(|key, _| !offset_key_in_ranges(&[(start.0, end.0)], key));
        self.overlay.deleted_ranges[index].push((start.0, end.0));
        Ok(())
    }

    fn commit(&self, sync: bool, submodule: &str) -> eyre::Result<()> {
        let mut batch = WriteBatch::new();
        for cf in Cf::ALL {
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
        let mut opts = WriteOptions::default();
        // `false` still appends the WAL. A later synced write or `flush_wal`
        // makes this group durable. `true` syncs the WAL, including earlier
        // unsynced records.
        opts.set_sync(sync);
        self.db
            .write_opt(batch, &opts)
            .wrap_err("commit submodule index batch")
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

    fn get_value<V: Decompress>(&self, cf: Cf, key: &[u8]) -> eyre::Result<Option<V>> {
        Ok(match self.get_raw(cf, key)? {
            Some(bytes) => Some(V::decompress(&bytes)?),
            None => None,
        })
    }

    fn cursor(&self, cf: Cf, start: Option<&[u8]>) -> eyre::Result<MergeCursor<'_>> {
        let handle = cf_handle(self.db, cf)?;
        let mode = match start {
            Some(key) => IteratorMode::From(key, Direction::Forward),
            None => IteratorMode::Start,
        };
        let hidden = self
            .overlay
            .map(|overlay| overlay.deleted_ranges[cf.index()].clone())
            .unwrap_or_default();
        let db = self
            .snap
            .iterator_cf(&handle, mode)
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
            .map(|overlay| overlay_pairs(overlay, cf, start))
            .unwrap_or_default();
        Ok(MergeCursor::new(Box::new(db), overlay, cleared))
    }

    fn collect_until<K, V>(
        &self,
        cf: Cf,
        start: Option<&[u8]>,
        mut stop: impl FnMut(&K) -> bool,
    ) -> eyre::Result<Vec<(K, V)>>
    where
        K: Decode,
        V: Decompress,
    {
        let mut cursor = self.cursor(cf, start)?;
        let mut rows = Vec::new();
        while let Some((key, value)) = cursor.next_kv()? {
            let key = K::decode(&key)?;
            if stop(&key) {
                break;
            }
            rows.push((key, V::decompress(&value)?));
        }
        Ok(rows)
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
            && overlay.maps[cf.index()].contains_key(&key)
        {
            // The overlay entry is a delete or it would have returned above.
            // Keep scanning the snapshot for a key the overlay does not hide.
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

    fn tx_hash_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<H256>> {
        let Some(row) = self.floor_interval(offset)? else {
            return Ok(None);
        };
        Ok((row.start <= offset && row.end >= offset).then_some(row.tx_path_hash))
    }

    fn floor_interval(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<IntervalRow>> {
        let Some((key, value)) = self.floor_raw(Cf::Interval, &encode_key(offset))? else {
            return Ok(None);
        };
        let start = PartitionChunkOffset::decode(&key)?;
        let value = TxPathInterval::decompress(&value)?;
        Ok(Some(IntervalRow {
            start,
            end: value.end,
            tx_path_hash: value.tx_path_hash,
        }))
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
            let interval = TxPathInterval::decompress(&value)?;
            if interval.end.0 < left_limit && offset < start {
                continue;
            }
            if offset.0 > right_limit {
                break;
            }
            rows.push(IntervalRow {
                start: offset,
                end: interval.end,
                tx_path_hash: interval.tx_path_hash,
            });
            if offset.0 >= right_limit {
                break;
            }
        }
        Ok(rows)
    }
}

fn overlay_pairs(
    overlay: &Overlay,
    cf: Cf,
    start: Option<&[u8]>,
) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let map = &overlay.maps[cf.index()];
    if let Some(start) = start {
        let start = start.to_vec();
        map.range(start..)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    } else {
        map.iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
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
    fn get_data_path_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkDataPath>> {
        self.get_value(Cf::DataPath, &encode_key(offset))
    }

    fn get_tx_path_by_offset(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<TxPath>> {
        let Some(hash) = self
            .get_path_hashes_by_offset(offset)?
            .and_then(|hashes| hashes.tx_path_hash)
        else {
            return Ok(None);
        };
        self.get_full_tx_path(hash)
    }

    fn get_path_hashes_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkPathHashes>> {
        let stored = self.get_value(Cf::PathHashes, &encode_key(offset))?;
        let tx_path_hash = self.tx_hash_at(offset)?;
        Ok(combine_hashes(stored, tx_path_hash))
    }

    fn path_hashes_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        if start > end {
            return Ok(Vec::new());
        }
        let intervals = self.intervals_touching(start, end)?;
        let start_key = encode_key(start);
        let stored = self.collect_until::<PartitionChunkOffset, ChunkPathHashes>(
            Cf::PathHashes,
            Some(&start_key),
            |offset| *offset > end,
        )?;
        let mut index = 0;
        let mut rows = Vec::new();
        for (offset, hashes) in stored {
            let Some(data_path_hash) = hashes.data_path_hash else {
                continue;
            };
            while index < intervals.len() && intervals[index].end < offset {
                index += 1;
            }
            let tx_path_hash = intervals.get(index).and_then(|interval| {
                (interval.start <= offset && interval.end >= offset)
                    .then_some(interval.tx_path_hash)
            });
            rows.push((
                offset,
                ChunkPathHashes {
                    data_path_hash: Some(data_path_hash),
                    tx_path_hash,
                },
            ));
        }
        Ok(rows)
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

    fn get_full_tx_path(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
        self.get_value(Cf::TxPath, &encode_key(path_hash))
    }

    fn get_tx_leaf_binding(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxLeafBinding>> {
        self.get_value(Cf::TxLeaf, &encode_key(path_hash))
    }

    fn get_data_root_infos_for_data_root(
        &self,
        data_root: DataRoot,
    ) -> eyre::Result<Option<DataRootInfos>> {
        self.get_value(Cf::DataRoots, &encode_key(data_root))
    }

    fn get_pending_body_migration(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<PendingBodyMigration>> {
        self.get_value(Cf::Pending, &encode_key(offset))
    }

    fn pending_body_migrations_from(
        &self,
        start: Option<PartitionChunkOffset>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        let start_key = start.map(encode_key);
        let mut remaining = limit.unwrap_or(usize::MAX);
        // `collect_until` asks before each row. Stop once `limit` rows are in.
        self.collect_until(Cf::Pending, start_key.as_deref(), |_| {
            if remaining == 0 {
                return true;
            }
            remaining -= 1;
            false
        })
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

    fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        // The key is a hash. It has no offset order. The value lists
        // placements, so the scan reads it. `fill_cache` stays off.
        let cleared = self
            .overlay
            .is_some_and(|overlay| overlay.cleared[Cf::DataRoots.index()]);
        let mut rows = BTreeMap::new();
        if !cleared {
            let handle = cf_handle(self.db, Cf::DataRoots)?;
            let mut opts = ReadOptions::default();
            opts.fill_cache(false);
            for item in self
                .snap
                .iterator_cf_opt(&handle, opts, IteratorMode::Start)
            {
                let (key, value) = item.map_err(|err| eyre::eyre!("{err}"))?;
                if self.overlay.is_some_and(|overlay| {
                    overlay.maps[Cf::DataRoots.index()].contains_key(key.as_ref())
                }) {
                    continue;
                }
                let root = DataRoot::decode(key.as_ref())?;
                rows.insert(root, DataRootInfos::decompress(value.as_ref())?);
            }
        }
        if let Some(overlay) = self.overlay {
            for (key, value) in &overlay.maps[Cf::DataRoots.index()] {
                let root = DataRoot::decode(key)?;
                match value {
                    Some(bytes) => {
                        rows.insert(root, DataRootInfos::decompress(bytes)?);
                    }
                    None => {
                        rows.remove(&root);
                    }
                }
            }
        }
        Ok(rows.into_iter().collect())
    }
}

impl SubmoduleRead for Batch<'_> {
    fn get_data_path_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkDataPath>> {
        self.rows().get_data_path_by_offset(offset)
    }

    fn get_tx_path_by_offset(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<TxPath>> {
        self.rows().get_tx_path_by_offset(offset)
    }

    fn get_path_hashes_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkPathHashes>> {
        self.rows().get_path_hashes_by_offset(offset)
    }

    fn path_hashes_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        self.rows().path_hashes_in_inclusive_range(start, end)
    }

    fn first_missing_path_hash_offset(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Option<PartitionChunkOffset>> {
        self.rows().first_missing_path_hash_offset(start, end)
    }

    fn missing_path_hash_ranges(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
        self.rows().missing_path_hash_ranges(start, end)
    }

    fn get_full_tx_path(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
        self.rows().get_full_tx_path(path_hash)
    }

    fn get_tx_leaf_binding(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxLeafBinding>> {
        self.rows().get_tx_leaf_binding(path_hash)
    }

    fn get_data_root_infos_for_data_root(
        &self,
        data_root: DataRoot,
    ) -> eyre::Result<Option<DataRootInfos>> {
        self.rows().get_data_root_infos_for_data_root(data_root)
    }

    fn get_pending_body_migration(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<PendingBodyMigration>> {
        self.rows().get_pending_body_migration(offset)
    }

    fn pending_body_migrations_from(
        &self,
        start: Option<PartitionChunkOffset>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        self.rows().pending_body_migrations_from(start, limit)
    }

    fn has_pending_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        self.rows().has_pending_in_inclusive_range(start, end)
    }

    fn tx_interval_overlaps(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        self.rows().tx_interval_overlaps(start, end)
    }

    fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        self.rows().data_root_entries()
    }
}

impl SubmoduleWrite for Batch<'_> {
    fn add_full_data_path(
        &mut self,
        offset: PartitionChunkOffset,
        data_path: ChunkDataPath,
    ) -> eyre::Result<()> {
        self.put(Cf::DataPath, &encode_key(offset), &data_path);
        Ok(())
    }

    fn add_full_tx_path(&mut self, path_hash: TxPathHash, tx_path: TxPath) -> eyre::Result<()> {
        self.put(Cf::TxPath, &encode_key(path_hash), &tx_path);
        Ok(())
    }

    fn add_tx_leaf_binding(
        &mut self,
        path_hash: TxPathHash,
        binding: &TxLeafBinding,
    ) -> eyre::Result<()> {
        self.put(Cf::TxLeaf, &encode_key(path_hash), binding);
        Ok(())
    }

    fn add_data_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<ChunkPathHash>,
    ) -> eyre::Result<()> {
        self.put_data_path_hash(offset, path_hash);
        Ok(())
    }

    fn add_tx_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        self.assign_tx_interval(offset, offset, path_hash)
    }

    fn add_tx_path_hash_to_offset_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        self.assign_tx_interval(start, end, path_hash)
    }

    fn write_data_path_updates(
        &mut self,
        mut updates: Vec<(PartitionChunkOffset, ChunkPathHash, ChunkDataPath)>,
    ) -> eyre::Result<()> {
        updates.sort_by_key(|(offset, _, _)| *offset);
        for (offset, path_hash, data_path) in updates {
            self.add_full_data_path(offset, data_path)?;
            self.put_data_path_hash(offset, Some(path_hash));
        }
        Ok(())
    }

    fn set_path_hashes_by_offset(
        &mut self,
        offset: PartitionChunkOffset,
        path_hashes: ChunkPathHashes,
    ) -> eyre::Result<()> {
        self.put_data_path_hash(offset, path_hashes.data_path_hash);
        self.assign_tx_interval(offset, offset, path_hashes.tx_path_hash)
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
        self.assign_tx_interval(start, end, None)?;
        self.delete_offset_range(Cf::PathHashes, start, end)?;
        self.delete_offset_range(Cf::DataPath, start, end)?;
        Ok(())
    }

    fn set_data_root_infos_for_data_root(
        &mut self,
        data_root: DataRoot,
        infos: DataRootInfos,
    ) -> eyre::Result<()> {
        let key = encode_key(data_root);
        if infos.0.is_empty() {
            self.delete(Cf::DataRoots, &key);
        } else {
            self.put(Cf::DataRoots, &key, &infos);
        }
        Ok(())
    }

    fn add_data_root_info(&mut self, data_root: DataRoot, info: &DataRootInfo) -> eyre::Result<()> {
        let mut infos = self
            .rows()
            .get_data_root_infos_for_data_root(data_root)?
            .unwrap_or_default();
        if !infos.0.contains(info) {
            infos.0.push(info.clone());
        }
        self.set_data_root_infos_for_data_root(data_root, infos)
    }

    fn add_pending_body_migration(
        &mut self,
        offset: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> eyre::Result<()> {
        self.put(Cf::Pending, &encode_key(offset), job);
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

    fn clear(&mut self) -> eyre::Result<()> {
        for cf in Cf::INDEX {
            self.overlay.cleared[cf.index()] = true;
            self.overlay.maps[cf.index()].clear();
            self.overlay.deleted_ranges[cf.index()].clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submodule::SubmoduleIndex;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, RelativeChunkOffset};

    #[test]
    fn reopen_reads_the_row_and_keeps_the_schema_across_clear() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_reopen")
            .build();
        let path = dir.path().join("index");
        let offset = PartitionChunkOffset::from(3);
        let hashes = ChunkPathHashes {
            data_path_hash: Some(H256::repeat_byte(1)),
            tx_path_hash: Some(H256::repeat_byte(2)),
        };
        {
            let store = RocksSubmoduleStore::open(&path)?;
            store.update(|tx| tx.set_path_hashes_by_offset(offset, hashes.clone()))?;
        }
        {
            let store = RocksSubmoduleStore::open(&path)?;
            assert_eq!(
                store.view(|tx| tx.get_path_hashes_by_offset(offset))?,
                Some(hashes)
            );
            store.update(|tx| tx.clear())?;
            let payload = store.schema_payload()?.expect("schema row");
            assert_eq!(payload, SCHEMA_TEXT.as_bytes());
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
        assert_eq!(marker, SCHEMA_TEXT);
        Ok(())
    }

    /// `clear` is one range tombstone per non-empty index family. It does not
    /// read values. 100k rows plus one large value must finish well under a scan.
    #[test]
    fn clear_of_a_large_index_leaves_only_metadata() -> eyre::Result<()> {
        const ROWS: u32 = 100_000;
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_clear_large")
            .build();
        let path = dir.path().join("index");
        let store = RocksSubmoduleStore::open(&path)?;
        let tx_hash = H256::repeat_byte(9);
        let data_root = H256::repeat_byte(4);
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
            tx.add_tx_leaf_binding(
                tx_hash,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::repeat_byte(1),
                },
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
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(10),
                Some(tx_hash),
            )?;
            tx.add_full_data_path(PartitionChunkOffset::from(0), vec![7_u8; 256 * 1024])?;
            Ok(())
        })?;
        assert!(store.cf_key_count(Cf::PathHashes)? >= usize::try_from(ROWS)?);
        assert!(store.cf_key_count(Cf::DataPath)? >= usize::try_from(ROWS)?);

        let started = std::time::Instant::now();
        store.update(|tx| tx.clear())?;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "clear took {elapsed:?}"
        );

        for cf in Cf::INDEX {
            assert_eq!(store.cf_key_count(cf)?, 0, "{}", cf.name());
        }
        assert_eq!(store.cf_key_count(Cf::Metadata)?, 1);
        let payload = store.schema_payload()?.expect("schema row");
        assert_eq!(payload, SCHEMA_TEXT.as_bytes());
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
        assert_eq!(store.cf_key_count(Cf::PathHashes)?, 0);
        assert_eq!(store.cf_key_count(Cf::Metadata)?, 1);
        assert_eq!(
            store.schema_payload()?.expect("schema row"),
            SCHEMA_TEXT.as_bytes()
        );
        Ok(())
    }

    #[test]
    fn v1_probe_reads_the_old_sst_and_leaves_the_schema() -> eyre::Result<()> {
        let dir = TempDirBuilder::new().prefix("submodule_rocks_v1").build();
        let path = dir.path().join("index");
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        let families = [
            rocksdb::ColumnFamilyDescriptor::new(LEGACY_PATH_HASH_CF, Options::default()),
            rocksdb::ColumnFamilyDescriptor::new("ChunkDataPathByPathHash", Options::default()),
        ];
        {
            let db =
                DB::open_cf_descriptors(&db_opts, &path, families).wrap_err("open v1 fixture")?;
            let handle = db.cf_handle(LEGACY_PATH_HASH_CF).expect("path hash family");
            db.put_cf(&handle, encode_key(PartitionChunkOffset::from(4)), b"row")
                .wrap_err("put v1 row")?;
            db.flush_cf(&handle).wrap_err("flush v1 fixture")?;
        }
        fs::write(path.join(SCHEMA_FILE), SCHEMA_V1_TEXT)?;

        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        let probe = RocksSubmoduleStore::open_v1_probe(&path, RocksTuning::baseline())?;
        assert!(probe.legacy_path_hash_present(PartitionChunkOffset::from(4))?);
        assert!(!probe.legacy_path_hash_present(PartitionChunkOffset::from(5))?);
        drop(probe);
        assert_eq!(fs::read_to_string(path.join(SCHEMA_FILE))?, SCHEMA_V1_TEXT);

        let probe = RocksSubmoduleStore::open_v1_probe(&path, RocksTuning::baseline())?;
        let before: Vec<_> = probe
            .legacy_path_hash_ssts()?
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert!(!before.is_empty(), "fixture produced no sst");
        probe.compact_legacy_path_hashes()?;
        let after = probe.legacy_path_hash_ssts()?;
        let after_names: Vec<_> = after.iter().map(|file| file.name.clone()).collect();
        assert_ne!(before, after_names, "compaction did not replace the sst");
        assert!(
            after
                .iter()
                .any(|file| file.start.is_some_and(|start| start <= 4)
                    && file.end.is_some_and(|end| end >= 4)),
            "{after:?}"
        );
        assert!(probe.legacy_path_hash_present(PartitionChunkOffset::from(4))?);
        drop(probe);
        assert_eq!(fs::read_to_string(path.join(SCHEMA_FILE))?, SCHEMA_V1_TEXT);
        let mut opts = String::new();
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("OPTIONS-") {
                opts.push_str(&fs::read_to_string(entry.path())?);
            }
        }
        assert!(opts.contains("partition_filters=true"), "{opts}");
        assert!(opts.contains("index_type=kTwoLevelIndexSearch"), "{opts}");
        assert!(
            opts.contains("pin_top_level_index_and_filter=true"),
            "{opts}"
        );
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

        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_presets")
            .build();
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
        assert!(
            baseline_opts.contains("recycle_log_file_num=4"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("wal_recovery_mode=kPointInTimeRecovery"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("partition_filters=true"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("index_type=kTwoLevelIndexSearch"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("pin_top_level_index_and_filter=true"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "enable_blob_files"
            )
            .as_deref(),
            Some("false"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "enable_blob_garbage_collection"
            )
            .as_deref(),
            Some("true"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "blob_garbage_collection_age_cutoff"
            )
            .as_deref(),
            Some("1.000000"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <TxPathByTxPathHash as Table>::NAME,
                "enable_blob_files"
            )
            .as_deref(),
            Some("false"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <TxPathByTxPathHash as Table>::NAME,
                "blob_garbage_collection_age_cutoff"
            )
            .as_deref(),
            Some("1.000000"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                <ChunkPathHashesByOffset as Table>::NAME,
                "enable_blob_garbage_collection"
            )
            .as_deref(),
            Some("false"),
            "{baseline_opts}"
        );
        let blobs_opts = persisted_options(&dir.path().join("blobs"))?;
        let min_blob = BLOB_MIN_BYTES.to_string();
        assert_eq!(
            cf_option(
                &blobs_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "enable_blob_files"
            )
            .as_deref(),
            Some("true"),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(
                &blobs_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "min_blob_size"
            )
            .as_deref(),
            Some(min_blob.as_str()),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(
                &blobs_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "enable_blob_garbage_collection"
            )
            .as_deref(),
            Some("true"),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(
                &blobs_opts,
                <ChunkDataPathByOffset as Table>::NAME,
                "blob_garbage_collection_age_cutoff"
            )
            .as_deref(),
            Some("0.250000"),
            "{blobs_opts}"
        );
        // An external block cache is not written to the OPTIONS file.
        // `cache_shard_bits` is checked on the preset above.
        let full = persisted_options(&dir.path().join("full-filters"))?;
        assert!(full.contains("partition_filters=false"), "{full}");
        assert!(!full.contains("index_type=kTwoLevelIndexSearch"), "{full}");
        let no_recycle_opts = persisted_options(&dir.path().join("no-wal-recycle"))?;
        assert!(
            no_recycle_opts.contains("recycle_log_file_num=0"),
            "{no_recycle_opts}"
        );
        assert!(
            no_recycle_opts.contains("wal_recovery_mode=kPointInTimeRecovery"),
            "{no_recycle_opts}"
        );
        assert!(
            no_recycle_opts.contains("partition_filters=true"),
            "{no_recycle_opts}"
        );
        assert!(baseline_opts.contains("use_fsync=true"), "{baseline_opts}");
        assert!(
            baseline_opts.contains("max_total_wal_size=0"),
            "{baseline_opts}"
        );
        let fdatasync_opts = persisted_options(&dir.path().join("fdatasync-wal"))?;
        assert!(
            fdatasync_opts.contains("use_fsync=false"),
            "{fdatasync_opts}"
        );
        assert!(
            fdatasync_opts.contains("max_total_wal_size=0"),
            "{fdatasync_opts}"
        );
        assert!(
            fdatasync_opts.contains("recycle_log_file_num=4"),
            "{fdatasync_opts}"
        );
        let fdatasync_rotate_opts = persisted_options(&dir.path().join("fdatasync-wal-rotate"))?;
        assert!(
            fdatasync_rotate_opts.contains("use_fsync=false"),
            "{fdatasync_rotate_opts}"
        );
        assert!(
            fdatasync_rotate_opts.contains(&format!("max_total_wal_size={WAL_ROTATE_BYTES}")),
            "{fdatasync_rotate_opts}"
        );
        let fsync_rotate_opts = persisted_options(&dir.path().join("fsync-wal-rotate"))?;
        assert!(
            fsync_rotate_opts.contains("use_fsync=true"),
            "{fsync_rotate_opts}"
        );
        assert!(
            fsync_rotate_opts.contains(&format!("max_total_wal_size={WAL_ROTATE_BYTES}")),
            "{fsync_rotate_opts}"
        );
        assert!(
            fsync_rotate_opts.contains("recycle_log_file_num=4"),
            "{fsync_rotate_opts}"
        );
        let rotate_compact_opts =
            persisted_options(&dir.path().join("fdatasync-wal-rotate-compact"))?;
        assert!(
            rotate_compact_opts.contains("use_fsync=false"),
            "{rotate_compact_opts}"
        );
        assert!(
            rotate_compact_opts.contains(&format!("max_total_wal_size={WAL_ROTATE_BYTES}")),
            "{rotate_compact_opts}"
        );
        Ok(())
    }

    /// An SST written with a bloom on every family still opens and reads when
    /// the next open leaves the always-hit families without a filter policy.
    /// The reverse open reads the same rows. New SSTs take the open's policy.
    #[test]
    fn bloom_scope_change_reopens_an_existing_db() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_bloom_scope")
            .build();
        let path = dir.path().join("index");
        let offset = PartitionChunkOffset::from(4);
        let path_bytes = b"data-path".to_vec();
        let tx_hash = H256::repeat_byte(9);
        let tx_bytes = b"tx-path".to_vec();
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
            store.update(|tx| {
                tx.add_full_data_path(offset, path_bytes.clone())?;
                tx.add_full_tx_path(tx_hash, tx_bytes.clone())?;
                tx.add_pending_body_migration(offset, &job)?;
                Ok(())
            })?;
            store.flush_memtables()?;
        }
        let written = persisted_options(&path)?;
        assert!(
            table_options_have_bloom(&written, <ChunkDataPathByOffset as Table>::NAME),
            "{written}"
        );
        assert!(
            table_options_have_bloom(&written, <TxPathByTxPathHash as Table>::NAME),
            "{written}"
        );

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            assert_eq!(
                store.view(|tx| tx.get_data_path_by_offset(offset))?,
                Some(path_bytes.clone())
            );
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(tx_hash))?,
                Some(tx_bytes.clone())
            );
            assert_eq!(
                store.view(|tx| tx.get_pending_body_migration(offset))?,
                Some(job.clone())
            );
            // Touch the memtables so the following flush writes an SST under
            // the baseline policy. The rows above already live in the old SST.
            store.update(|tx| tx.add_full_data_path(PartitionChunkOffset::from(5), vec![1]))?;
            store.flush_memtables()?;
        }
        let reopened = persisted_options(&path)?;
        assert!(
            !table_options_have_bloom(&reopened, <ChunkDataPathByOffset as Table>::NAME),
            "{reopened}"
        );
        assert!(
            !table_options_have_bloom(&reopened, <PendingBodyMigrationsByOffset as Table>::NAME),
            "{reopened}"
        );
        assert!(
            table_options_have_bloom(&reopened, <TxPathByTxPathHash as Table>::NAME),
            "{reopened}"
        );
        assert!(
            table_options_have_bloom(&reopened, <ChunkPathHashesByOffset as Table>::NAME),
            "{reopened}"
        );

        let store = RocksSubmoduleStore::open_with(&path, RocksTuning::preset("all-blooms")?)?;
        assert_eq!(
            store.view(|tx| tx.get_data_path_by_offset(offset))?,
            Some(path_bytes)
        );
        assert_eq!(
            store.view(|tx| tx.get_full_tx_path(tx_hash))?,
            Some(tx_bytes)
        );
        assert_eq!(
            store.view(|tx| tx.get_pending_body_migration(offset))?,
            Some(job)
        );
        Ok(())
    }

    fn table_options_have_bloom(opts: &str, cf: &str) -> bool {
        let header = format!("[TableOptions/BlockBasedTable \"{cf}\"]");
        let Some(start) = opts.find(&header) else {
            return false;
        };
        let rest = &opts[start + header.len()..];
        let section = match rest.find("\n[") {
            Some(end) => &rest[..end],
            None => rest,
        };
        section.contains("filter_policy=bloomfilter")
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
    /// the column options are built, including the opens inside convert and
    /// verify.
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
    /// Forced compaction inlines them. Reconcile and converter verify run
    /// while legacy blob values and new inline values are both present.
    #[test]
    fn legacy_blob_values_reopen_inline_and_compact_away() -> eyre::Result<()> {
        use crate::IrysDatabaseArgs as _;
        use crate::submodule::{ConvertOptions, convert_rocks_to_mdbx, verify_mdbx_and_rocks};
        use reth_db::mdbx::DatabaseArguments;

        let _hold = HoldAutoCompaction::enter();
        const LEGACY_PATHS: u32 = 4_096;
        const INLINE_PATHS: u32 = 256;

        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_legacy_blob")
            .build();
        let path = dir.path().join("index");
        let legacy_tx = H256::repeat_byte(1);
        let inline_tx = H256::repeat_byte(2);
        let total = LEGACY_PATHS + INLINE_PATHS;
        let removed = [0_u32, LEGACY_PATHS];

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::preset("blobs")?)?;
            write_large_paths(&store, 0, LEGACY_PATHS)?;
            store.update(|tx| tx.add_full_tx_path(legacy_tx, large_path(0)))?;
            store.flush_memtables()?;
            assert!(
                store.blob_file_count(Cf::DataPath)? > 0,
                "data paths did not reach blob files"
            );
            assert!(
                store.blob_file_count(Cf::TxPath)? > 0,
                "tx path did not reach a blob file"
            );
        }

        {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            assert!(store.blob_file_count(Cf::DataPath)? > 0);
            assert!(store.blob_file_count(Cf::TxPath)? > 0);
            assert_data_paths(&store, LEGACY_PATHS, &[])?;
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(legacy_tx))?,
                Some(large_path(0))
            );

            write_large_paths(&store, LEGACY_PATHS, INLINE_PATHS)?;
            store.update(|tx| tx.add_full_tx_path(inline_tx, large_path(1)))?;
            store.flush_memtables()?;
            assert!(
                store.blob_file_count(Cf::DataPath)? > 0,
                "flush of inline values dropped legacy blob files"
            );
            assert!(store.blob_file_count(Cf::TxPath)? > 0);
            assert_data_paths(&store, total, &[])?;
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(inline_tx))?,
                Some(large_path(1))
            );

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
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(legacy_tx))?,
                Some(large_path(0))
            );
        }

        let mdbx = dir.path().join("db");
        let options = ConvertOptions::new(DatabaseArguments::irys_testing()?);
        let copied = convert_rocks_to_mdbx(&path, &mdbx, &options)?;
        assert_eq!(copied.data_paths, u64::from(total - 2));
        assert_eq!(copied.tx_paths, 2);
        let report = verify_mdbx_and_rocks(&mdbx, &path, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(report.offsets_checked, u64::from(total - 2), "{report}");
        assert_eq!(report.tx_paths_checked, 2, "{report}");

        let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
        assert!(
            store.blob_file_count(Cf::DataPath)? > 0,
            "verify open compacted legacy blobs away"
        );
        assert!(store.blob_file_count(Cf::TxPath)? > 0);
        store.compact_column_force(Cf::DataPath)?;
        store.compact_column_force(Cf::TxPath)?;
        assert_eq!(store.blob_file_count(Cf::DataPath)?, 0);
        assert_eq!(store.blob_file_count(Cf::TxPath)?, 0);
        assert_data_paths(&store, total, &removed)?;
        assert_eq!(
            store.view(|tx| tx.get_full_tx_path(legacy_tx))?,
            Some(large_path(0))
        );
        assert_eq!(
            store.view(|tx| tx.get_full_tx_path(inline_tx))?,
            Some(large_path(1))
        );
        Ok(())
    }

    #[test]
    fn small_file_compact_stays_off_when_the_threshold_is_zero() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_compact_off")
            .build();
        let store =
            RocksSubmoduleStore::open_with(dir.path().join("index"), RocksTuning::baseline())?;
        store.disable_auto_compaction()?;
        let rows = 20_u32;
        for offset in 0..rows {
            put_offset(&store, offset, offset as u8)?;
            store.flush_memtables()?;
        }
        let files = store.path_hash_sst_count();
        assert!(
            files > 8,
            "expected many tiny SSTs after per-row flushes, got {files}"
        );
        std::thread::sleep(Duration::from_millis(400));
        let stats = store.ingest_stats()?;
        assert_eq!(stats.small_file_compactions, 0);
        assert!(
            store.path_hash_sst_count() > 8,
            "threshold 0 must leave the tiny SSTs in place, started with {files}"
        );
        for offset in 0..rows {
            assert_eq!(
                path_hashes_at(&store, offset)?,
                Some(hashes_for(offset as u8))
            );
        }
        Ok(())
    }

    #[test]
    fn small_file_compact_merges_tiny_ssts_past_the_threshold() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_compact_on")
            .build();
        let path = dir.path().join("index");
        let rows = 20_u32;
        let files = {
            let store = RocksSubmoduleStore::open_with(&path, RocksTuning::baseline())?;
            store.disable_auto_compaction()?;
            for offset in 0..rows {
                put_offset(&store, offset, offset as u8)?;
                store.flush_memtables()?;
            }
            let files = store.path_hash_sst_count();
            assert!(
                files > 8,
                "expected many tiny SSTs after per-row flushes, got {files}"
            );
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
            let after = store.path_hash_sst_count();
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
        for offset in 0..rows {
            assert_eq!(
                path_hashes_at(&store, offset)?,
                Some(hashes_for(offset as u8))
            );
        }
        Ok(())
    }

    /// Writes, flushes until a retired WAL is reused, then reopens.
    /// The last row is only in that WAL.
    #[test]
    fn wal_recycle_reopens_after_a_reused_log() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_wal_recycle")
            .build();
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
                "Cumulative writes: 1\nCumulative stall: 01:02:03.250 H:M:S, 9.0 percent\n"
            ),
            Some(3_723_250_000)
        );
        assert!(parse_cumulative_stall_micros("no stall line").is_none());

        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_stats")
            .build();
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

    #[test]
    fn production_open_keeps_tables_and_tickers() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_stats")
            .build();
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
    /// about 1 MiB. Forty databases and nine families are about 360 MiB.
    /// That fits in the 512 MiB manager and in the 1 GiB cache. A smaller
    /// manager cannot hold the idle arenas.
    #[test]
    fn forty_databases_share_one_cache_and_write_buffer() -> eyre::Result<()> {
        let limits = RocksResourceLimits::production();
        let resources = RocksResources::new(limits)?;
        let before = rocksdb_thread_count();
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_shared")
            .build();
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
        let dir = TempDirBuilder::new().prefix("submodule_rocks_rate").build();
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
        ChunkPathHashes {
            data_path_hash: Some(H256::repeat_byte(byte)),
            tx_path_hash: Some(H256::repeat_byte(byte.wrapping_add(1))),
        }
    }

    fn put_offset(store: &RocksSubmoduleStore, offset: u32, byte: u8) -> eyre::Result<()> {
        store.update(|tx| {
            tx.set_path_hashes_by_offset(PartitionChunkOffset::from(offset), hashes_for(byte))
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

    fn persisted_options(path: &Path) -> eyre::Result<String> {
        let mut found = None;
        for entry in fs::read_dir(path).wrap_err("read db dir")? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("OPTIONS-") {
                found = Some(fs::read_to_string(entry.path()).wrap_err("read OPTIONS")?);
            }
        }
        found.ok_or_else(|| eyre::eyre!("no OPTIONS file in {}", path.display()))
    }
}
