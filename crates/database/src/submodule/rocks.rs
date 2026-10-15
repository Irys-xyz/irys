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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
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
use reth_db::table::{Decode, Decompress, Encode, Table};
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
    /// with `disableWAL` is rejected while the count is non-zero. Production
    /// writes keep the WAL. The converting open sets this count to 0 so its
    /// bulk load can disable the WAL. `WAL_ttl_seconds` and
    /// `WAL_size_limit_MB` are unset; either one would also force the count
    /// to 0.
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
    /// This store has the default family plus six user families, including
    /// `ChunkOffsetsByContentHash`.
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
    /// `ChunkOffsetsByContentHash` is not controlled by this flag. A lookup
    /// seeks the 32-byte content hash. The offset suffix is not known, so a
    /// whole-key bloom cannot answer it. That family always uses a fixed
    /// 32-byte prefix extractor and a prefix bloom (`whole_key_filtering`
    /// off), full filter, 10 bits per prefix. Partitioned filters stay on
    /// in production so one filter block can be read without the whole SST
    /// filter.
    ///
    /// Does not change `partition_filters`. An existing SST keeps its own
    /// filter until compaction rewrites the file.
    pub bloom_all_families: bool,
    /// Bloom on `ChunkMetaByOffset`. A proof write reads the stored row first,
    /// and a new offset misses. Off by default: a full-partition measurement
    /// did not change proof-write time. The `datapath-bloom` preset turns it on.
    pub data_path_bloom: bool,
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

    /// Point lookups on this family can miss, so a bloom can reject a block.
    ///
    /// `bloom_all_families` forces the filter on the other families too.
    /// [`Self::Content`] always has a prefix bloom. Data-root reads are
    /// prefix seeks. A whole-key bloom cannot answer them. See
    /// [`RocksTuning::bloom_all_families`].
    fn uses_bloom(self, tuning: &RocksTuning) -> bool {
        matches!(self, Self::Content)
            || tuning.bloom_all_families
            || (tuning.data_path_bloom && matches!(self, Self::OffsetMeta))
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
    /// Chosen from the SCHEMA file before `DB::open`. An empty directory is RocksDB v1.
    layout: IndexLayout,
    /// A read-only open refuses every write, including compaction and the schema stamp.
    read_only: bool,
    /// Conversion open. Writes skip the WAL. [`Self::seal_and_stamp`] flushes
    /// the memtables, drops the log files, and writes the SCHEMA file last.
    bulk_load: bool,
    /// Exclusive lock of the RocksDB `LOCK` file. Drop releases it.
    ///
    /// Read-only open does not take that lock itself. The converter holds it
    /// so a node cannot open the same directory for write during the read.
    /// The field is read by drop. Nothing else inspects it.
    #[expect(dead_code, reason = "held only so drop releases the lock")]
    lock_file: Option<Arc<File>>,
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
    pub(super) fn open_converting(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let mut tuning = RocksTuning::baseline();
        tuning.recycle_log_file_num = 0;
        let mut opened = Self::open_inner(
            path,
            tuning,
            true,
            false,
            None,
            false,
            Some(IndexLayout::RocksV1),
        )?;
        opened.bulk_load = true;
        Ok(opened)
    }

    /// Read-only open of a completed index. Does not create families or write the schema.
    ///
    /// Flocks `LOCK` when that file exists and holds it until this store drops.
    /// A missing `LOCK` means nothing holds it; this function does not create one.
    pub(super) fn open_read_only(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.join(SCHEMA_FILE).is_file() {
            eyre::bail!(
                "rocks index {} has no SCHEMA marker. Refusing a read-only open of an incomplete directory.",
                path.display()
            );
        }
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
        })
    }

    /// Flush copied rows into SST files, drop WAL logs, then write SCHEMA.
    ///
    /// The database is closed before the log files are removed, and the
    /// SCHEMA file is written after that. A crash before the file leaves no
    /// completion marker, so the run is redone. A family with more than one
    /// SST is compacted so the finished directory is not a pile of flushes.
    pub(super) fn seal_and_stamp(self) -> eyre::Result<()> {
        let path = self.path.clone();
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
        drop(self);
        remove_rocks_wal_files(&path)?;
        sync_dir(&path)?;
        write_schema_marker_file(&path, SUBMODULE_SCHEMA_ROCKS_V1)
    }

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
        // A prefix extractor stops `Next` unless the seek is total order.
        opts.set_total_order_seek(true);
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
            if key.len() != 4 {
                continue;
            }
            let start = u32::from_be_bytes(key.as_ref().try_into().expect("length checked"));
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
        let (_key, value) = item.wrap_err("read the last placement")?;
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
        batch.commit(sync && !disable_wal, disable_wal, &self.submodule)?;
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
        for cf in families() {
            let Ok(handle) = cf_handle(&self.db, cf) else {
                continue;
            };
            let name = if matches!(cf, Cf::OffsetMeta) {
                self.offset_family_name()
            } else {
                cf.name()
            };
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
        for cf in families() {
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
        for cf in families() {
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
    /// Counts keys only, then `DeleteRange` removes them. RocksDB v1 also
    /// reads each data-proof value in a span that has rows and point-deletes
    /// its content-hash key in the same batch. A span with no presence key
    /// does not enter the batch and does not read values. An empty result
    /// does not write, so the sequence stays put.
    pub(super) fn delete_presence_ranges(
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
        let disable_wal = self.bulk_load;
        self.db
            .write_opt(batch, &write_opts(true, disable_wal))
            .wrap_err("reconcile presence rows")?;
        if !disable_wal {
            self.group.note_wal_sync();
            self.group.cover_durable();
        }
        Ok(PresenceReconcile {
            path_hash_rows,
            data_path_rows,
        })
    }

    /// Drop proof rows in each span. Keep a checksum-only row.
    ///
    /// A proof row's checksum was computed over data bytes. After the span
    /// is entropy those bytes are gone, so the row is dropped and the offset
    /// is unknown until the next entropy write.
    fn retain_entropy_crc_rows(
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
        let disable_wal = self.bulk_load;
        self.db
            .write_opt(batch, &write_opts(true, disable_wal))
            .wrap_err("drop entropy proof rows")?;
        if !disable_wal {
            self.group.note_wal_sync();
            self.group.cover_durable();
        }
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

    #[cfg(test)]
    fn schema_payload(&self) -> eyre::Result<Option<Vec<u8>>> {
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let key = encode_key(MetadataKey::DBSchemaVersion);
        match self.db.get_cf(&handle, key).wrap_err("read schema row")? {
            Some(bytes) => Ok(Some(bytes)),
            None => Ok(None),
        }
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
        if !self.stores_offset_crc() {
            return Ok(false);
        }
        Ok(self
            .read_raw_metadata(MetadataKey::ChunkCrcComplete)?
            .is_some())
    }

    fn set_chunk_crc_complete(&self, complete: bool) -> eyre::Result<()> {
        if !self.stores_offset_crc() {
            return Ok(());
        }
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
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let mut batch = WriteBatch::new();
        batch.put_cf(&handle, encode_key(key), value);
        let disable_wal = self.bulk_load;
        self.db
            .write_opt(batch, &write_opts(true, disable_wal))
            .wrap_err("write submodule metadata")?;
        if !disable_wal {
            self.group.note_wal_sync();
            self.group.cover_durable();
        }
        Ok(())
    }

    /// Synced metadata write. The value is raw. Same WAL barrier as `update`.
    /// Not part of `clear`.
    fn write_metadata(&self, key: MetadataKey, value: Option<Vec<u8>>) -> eyre::Result<()> {
        self.ensure_writable()?;
        let _guard = self.lock_write();
        self.ensure_writable()?;
        let handle = cf_handle(&self.db, Cf::Metadata)?;
        let mut batch = WriteBatch::new();
        let encoded = encode_key(key);
        match &value {
            Some(bytes) => {
                batch.put_cf(&handle, encoded, bytes);
            }
            None => batch.delete_cf(&handle, encoded),
        }
        let disable_wal = self.bulk_load;
        self.db
            .write_opt(batch, &write_opts(true, disable_wal))
            .wrap_err("write submodule metadata")?;
        if !disable_wal {
            self.group.note_wal_sync();
            self.group.cover_durable();
        }
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
    // Full filter, 10 bits per key. `false` is not a block-based filter.
    // Only families whose point Gets can miss pay for one. A hit still
    // reads the value, and an iterator seek never consults a filter, so
    // the filter is a second I/O and a cache resident on those families.
    //
    // Offset, interval, pending, and data-root reads seek a key the caller
    // already has. A bloom there is a second read and a pinned L0 filter.
    // Content is the miss: the lookup knows the hash and not the offset.
    // Partitioned filters are the production shape. See
    // `RocksTuning::partition_filters`. `all-blooms` restores the filter
    // on the other families.
    let bloom = cf.uses_bloom(tuning);
    if bloom {
        table.set_bloom_filter(10.0, false);
    }
    // A content-hash lookup seeks the 32-byte prefix. The offset suffix is
    // not known. A whole-key bloom stores hash‖offset, so it cannot answer
    // that seek. This family uses a fixed 32-byte prefix extractor and a
    // prefix bloom (`whole_key_filtering` off). The filter is a full filter,
    // 10 bits per prefix, the same density as the other miss families.
    // Partitioned filters stay on in production so one filter block can be
    // read without the whole SST filter. An iterator that must see every
    // prefix sets `total_order_seek`. A prefix seek leaves that off and sets
    // `prefix_same_as_start`, which is what consults this bloom.
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

/// A non-empty directory without SCHEMA is not a new database.
///
/// `CONVERTING` is an unfinished conversion. `CURRENT` without that file is
/// a first open that crashed before the marker. A directory that already has
/// SCHEMA is complete: `CONVERTING` beside it is the crash after the marker
/// and before the sentinel is removed, and open continues.
fn refuse_incomplete_directory(path: &Path) -> eyre::Result<()> {
    if path.join(SCHEMA_FILE).exists() {
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

/// Stamp 24 zero bytes when the row is absent on a writable open.
///
/// A present row is checked and left as stored. Read-only open does not write.
fn ensure_feature_flags(
    db: &DB,
    path: &Path,
    writable: bool,
    disable_wal: bool,
) -> eyre::Result<()> {
    let handle = cf_handle(db, Cf::Metadata)?;
    let key = encode_key(MetadataKey::FeatureFlags);
    match db.get_cf(&handle, &key).wrap_err("read feature flags")? {
        Some(bytes) => {
            let flags = parse_feature_flags(&bytes)?;
            refuse_unknown_feature_bits(path, &flags, writable)
        }
        None if writable => {
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

/// Leaf `data_hash` of a data proof. A value shorter than a leaf is not a
/// content-addressed proof and has no reverse row. The hash is the leaf
/// already stored in the proof. Chunk bytes are not hashed again.
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

    fn put_data_path_hash(
        &mut self,
        _offset: PartitionChunkOffset,
        _path_hash: Option<ChunkPathHash>,
    ) {
        // RocksDB v1 has no path-hash family. A proof in the offset row is the
        // presence bit. A checksum-only row is not.
    }

    fn assign_tx_interval(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        tx_path_hash: Option<H256>,
    ) -> eyre::Result<()> {
        self.assign_placement(start, end, tx_path_hash)
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
    /// only. It does not delete the prefix.
    fn delete_content_rows_for_paths(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let start_key = encode_key(start);
        let proofs = self
            .rows()
            .collect_raw(Cf::OffsetMeta, Some(&start_key), |key| {
                PartitionChunkOffset::decode(key)
                    .map(|offset| offset > end)
                    .unwrap_or(true)
            })?;
        for (key, value) in proofs {
            let Ok(offset) = PartitionChunkOffset::decode(&key) else {
                continue;
            };
            let meta = stored_offset_meta(&value)?;
            let Some(hash) = meta.proof.as_deref().and_then(content_hash_of) else {
                continue;
            };
            self.delete(Cf::Content, &records::content_key(&hash, offset));
        }
        Ok(())
    }

    fn commit(&self, sync: bool, disable_wal: bool, submodule: &str) -> eyre::Result<()> {
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
        self.db
            .write_opt(batch, &write_opts(sync, disable_wal))
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

    fn cursor(&self, cf: Cf, start: Option<&[u8]>) -> eyre::Result<MergeCursor<'_>> {
        let handle = cf_handle(self.db, cf)?;
        // A prefix extractor stops `Next` at the first prefix unless the
        // iterator is total order. Prefix lookups do not use this cursor.
        // The block cache stays on: pending and data-root reads are hot.
        let mut opts = ReadOptions::default();
        opts.set_total_order_seek(true);
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
            .map(|overlay| overlay_pairs(overlay, cf, start))
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
            key.len() < 32 || key[..32] != prefix
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
        mut stop: impl FnMut(&[u8]) -> bool,
    ) -> eyre::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut cursor = self.cursor(cf, start)?;
        let mut rows = Vec::new();
        while let Some((key, value)) = cursor.next_kv()? {
            if stop(&key) {
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
            PartitionChunkOffset::decode(key)
                .map(|offset| offset > end)
                .unwrap_or(true)
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
                return true;
            }
            remaining -= 1;
            false
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
        let raw = self.collect_raw(Cf::DataRoots, None, |_| false)?;
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
            PartitionChunkOffset::decode(key)
                .map(|offset| offset > end)
                .unwrap_or(true)
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
        // Streams: a partition holds tens of millions of rows.
        let start_key = encode_key(start);
        let mut cursor = self.cursor(Cf::OffsetMeta, Some(&start_key))?;
        let mut expect = u64::from(start.0);
        let last = u64::from(end.0);
        while let Some((key, bytes)) = cursor.next_kv()? {
            let offset = u64::from(PartitionChunkOffset::decode(&key)?.0);
            if offset > last {
                break;
            }
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

impl SubmoduleRead for Batch<'_> {
    fn stores_tx_paths(&self) -> bool {
        false
    }

    fn stores_offset_crc(&self) -> bool {
        self.rows().stores_offset_crc()
    }

    fn get_data_path_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkDataPath>> {
        self.rows().get_data_path_by_offset(offset)
    }

    fn get_offset_meta(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<OffsetMeta>> {
        self.rows().get_offset_meta(offset)
    }

    fn offset_metas_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, OffsetMeta)>> {
        self.rows().offset_metas_in_range(start, end)
    }

    fn offset_crc_missing(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        self.rows().offset_crc_missing(start, end)
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

    fn has_pending_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<bool> {
        self.rows().has_pending_in_ranges(ranges)
    }

    fn tx_intervals_overlap_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<bool> {
        self.rows().tx_intervals_overlap_ranges(ranges)
    }

    fn tx_interval_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<IntervalRow>> {
        self.rows().interval_covering(offset)
    }

    fn tx_intervals_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<IntervalRow>> {
        self.rows().intervals_overlapping(start, end)
    }

    fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        self.rows().data_root_entries()
    }

    fn placement_at(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<Placement>> {
        self.rows().placement_at(offset)
    }

    fn placements_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<Placement>> {
        self.rows().placements_in_range(start, end)
    }

    fn placements_for_data_root(&self, data_root: DataRoot) -> eyre::Result<Vec<Placement>> {
        self.rows().placements_for_data_root(data_root)
    }

    fn offsets_for_content_hash(
        &self,
        content_hash: H256,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        self.rows().offsets_for_content_hash(content_hash)
    }
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
            tracing::warn!(
                offset = offset.0,
                "data path has no covering interval; the proof was not stored"
            );
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
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        updates.sort_by_key(|(offset, _, _)| *offset);
        let mut uncovered = Vec::new();
        for (offset, path_hash, data_path) in updates {
            // Skip the path write too. `add_full_data_path` would drop it and
            // warn again, and a hash with no path is not a stored proof.
            if self.layout == IndexLayout::RocksV1
                && self.rows().interval_covering(offset)?.is_none()
            {
                tracing::warn!(
                    offset = offset.0,
                    "data path has no covering interval; the proof was not stored"
                );
                uncovered.push(offset);
                continue;
            }
            self.add_full_data_path(offset, data_path)?;
            self.put_data_path_hash(offset, Some(path_hash));
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
                tracing::warn!(
                    offset = row.offset.0,
                    "data path has no covering interval; the proof was not stored"
                );
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

        // `delete_offset_range` hides these keys from a later read.
        // Pending jobs for the cleared placement go in this same batch.
        self.delete_content_rows_for_paths(start, end)?;
        self.delete_offset_range(Cf::Pending, start, end)?;

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
        let dir = TempDirBuilder::new().prefix("submodule_span_pass").build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_reopen")
            .build();
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
            let payload = store.schema_payload()?.expect("schema row");
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

    /// A marker that is not RocksDB v1 is refused. Open writes nothing.
    #[test]
    fn foreign_marker_is_refused_and_writes_nothing() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_foreign")
            .build();
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
    /// Open does not create the family.
    #[test]
    fn missing_known_family_is_refused_and_not_created() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_missing_cf")
            .build();
        let path = dir.path().join("index");
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        let families = [
            "default",
            "PlacementIntervals",
            "DataRootPlacements",
            "PendingBodyMigrations",
            "Metadata",
            "ChunkOffsetsByContentHash",
        ]
        .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, Options::default()));
        {
            let _db = DB::open_cf_descriptors(&db_opts, &path, families)?;
        }
        fs::write(path.join(SCHEMA_FILE), SUBMODULE_SCHEMA_ROCKS_V1)?;
        let before = DB::list_cf(&Options::default(), &path)?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(CHUNK_META_BY_OFFSET_CF), "{text}");
        assert!(text.contains("does not create it empty"), "{text}");
        let after = DB::list_cf(&Options::default(), &path)?;
        assert_eq!(before, after);
        assert!(!after.iter().any(|name| name == CHUNK_META_BY_OFFSET_CF));
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_feature_stamp")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_compat")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_unknown_cf_compact")
            .build();
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

    /// Any incompat bit this binary does not define refuses read and write.
    /// The stored bytes stay.
    #[test]
    fn unknown_incompat_bit_refuses_and_keeps_the_row() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_incompat")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_ro_compat")
            .build();
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

    /// An existing database that lacks any RocksDB v1 family is refused.
    /// Open does not create the family.
    #[test]
    fn missing_placement_family_is_refused_and_not_created() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_missing_placement")
            .build();
        let path = dir.path().join("index");
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        let families = [
            "default",
            "DataRootPlacements",
            "PendingBodyMigrations",
            "Metadata",
            "ChunkOffsetsByContentHash",
            CHUNK_META_BY_OFFSET_CF,
        ]
        .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, reopen_options(name)));
        {
            let _db = DB::open_cf_descriptors(&db_opts, &path, families)?;
        }
        fs::write(path.join(SCHEMA_FILE), SUBMODULE_SCHEMA_ROCKS_V1)?;
        let before = DB::list_cf(&Options::default(), &path)?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("PlacementIntervals"), "{text}");
        assert!(text.contains("does not create it empty"), "{text}");
        let after = DB::list_cf(&Options::default(), &path)?;
        assert_eq!(before, after);
        assert!(!after.iter().any(|name| name == "PlacementIntervals"));
        Ok(())
    }

    /// `CONVERTING` and no `SCHEMA` is an unfinished conversion. Open writes nothing.
    #[test]
    fn converting_only_directory_refuses_and_writes_nothing() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_converting")
            .build();
        let path = dir.path().join("index");
        fs::create_dir_all(&path)?;
        let sentinel = b"irys-submodule-index convert-incomplete\n";
        fs::write(path.join("CONVERTING"), sentinel)?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("with --replace-incomplete"), "{text}");
        assert!(text.contains("SCHEMA"), "{text}");
        assert_eq!(fs::read(path.join("CONVERTING"))?, sentinel);
        let mut names = Vec::new();
        for entry in fs::read_dir(&path)? {
            names.push(entry?.file_name());
        }
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from("CONVERTING")]);
        Ok(())
    }

    /// A stray file and no `SCHEMA` is not a new database.
    #[test]
    fn non_empty_dir_without_schema_refuses_and_writes_nothing() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_stray")
            .build();
        let path = dir.path().join("index");
        fs::create_dir_all(&path)?;
        fs::write(path.join("notes"), b"stray")?;
        let err = RocksSubmoduleStore::open(&path).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("with --replace-incomplete"), "{text}");
        assert!(text.contains("SCHEMA"), "{text}");
        assert_eq!(fs::read(path.join("notes"))?, b"stray");
        assert!(!path.join("CURRENT").is_file());
        assert!(!path.join(SCHEMA_FILE).exists());
        Ok(())
    }

    /// `CURRENT` without `SCHEMA` and without `CONVERTING` is a crashed first open.
    /// `--replace-incomplete` does not delete that directory.
    #[test]
    fn crashed_first_open_names_delete_not_replace_incomplete() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_fresh_crash")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_schema_converting")
            .build();
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

    /// A RocksDB v1 proof outside every interval is not stored. A covered
    /// offset in the same batch still is. The skipped offset is the result.
    #[test]
    fn uncovered_data_path_is_reported_and_not_stored() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_uncovered_path")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_clear_large")
            .build();
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
        let payload = store.schema_payload()?.expect("schema row");
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
            store.schema_payload()?.expect("schema row"),
            SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()
        );
        Ok(())
    }

    #[test]
    fn content_hash_index_tracks_writes_rewrites_and_deletes() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_content_index")
            .build();
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

    #[test]
    fn checksum_only_row_is_not_a_stored_proof() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_crc_only")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_content_order")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_no_leak")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_owned")
            .build();
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
                .is_none()
        );
        assert_no_orphans(&store)?;
        Ok(())
    }

    #[test]
    fn clear_in_a_batch_does_not_count_snapshot_keys() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_cleared_count")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_range_hides")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_packing")
            .build();
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_raw_flags")
            .build();
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
            cf_option(&baseline_opts, CHUNK_META_BY_OFFSET_CF, "enable_blob_files").as_deref(),
            Some("false"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                CHUNK_META_BY_OFFSET_CF,
                "enable_blob_garbage_collection"
            )
            .as_deref(),
            Some("true"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                CHUNK_META_BY_OFFSET_CF,
                "blob_garbage_collection_age_cutoff"
            )
            .as_deref(),
            Some("1.000000"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("[CFOptions \"PlacementIntervals\"]"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("[CFOptions \"DataRootPlacements\"]"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("[CFOptions \"PendingBodyMigrations\"]"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("[CFOptions \"ChunkMetaByOffset\"]"),
            "{baseline_opts}"
        );
        assert!(
            !baseline_opts.contains("ChunkDataPathByOffset"),
            "{baseline_opts}"
        );
        assert!(
            baseline_opts.contains("[CFOptions \"ChunkOffsetsByContentHash\"]"),
            "{baseline_opts}"
        );
        assert_eq!(
            cf_option(
                &baseline_opts,
                "ChunkOffsetsByContentHash",
                "prefix_extractor"
            )
            .as_deref(),
            Some("rocksdb.FixedPrefix.32"),
            "{baseline_opts}"
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
        assert!(
            !baseline_opts.contains("TxPathByTxPathHash"),
            "{baseline_opts}"
        );
        assert!(
            !baseline_opts.contains("ChunkPathHashesByOffset"),
            "{baseline_opts}"
        );
        let blobs_opts = persisted_options(&dir.path().join("blobs"))?;
        let min_blob = BLOB_MIN_BYTES.to_string();
        assert_eq!(
            cf_option(&blobs_opts, CHUNK_META_BY_OFFSET_CF, "enable_blob_files").as_deref(),
            Some("true"),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(&blobs_opts, CHUNK_META_BY_OFFSET_CF, "min_blob_size").as_deref(),
            Some(min_blob.as_str()),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(
                &blobs_opts,
                CHUNK_META_BY_OFFSET_CF,
                "enable_blob_garbage_collection"
            )
            .as_deref(),
            Some("true"),
            "{blobs_opts}"
        );
        assert_eq!(
            cf_option(
                &blobs_opts,
                CHUNK_META_BY_OFFSET_CF,
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
    /// `ChunkOffsetsByContentHash` keeps its prefix bloom on every open.
    #[test]
    fn bloom_scope_change_reopens_an_existing_db() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_bloom_scope")
            .build();
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

        let dir = TempDirBuilder::new().prefix("submodule_rocks_blob").build();
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
        let files = store.sst_file_count(Cf::Interval)?;
        assert!(
            files > 8,
            "expected many tiny SSTs after per-row flushes, got {files}"
        );
        std::thread::sleep(Duration::from_millis(400));
        let stats = store.ingest_stats()?;
        assert_eq!(stats.small_file_compactions, 0);
        assert!(
            store.sst_file_count(Cf::Interval)? > 8,
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
            let files = store.sst_file_count(Cf::Interval)?;
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
                "Cumulative writes: 1\nCumulative stall: 01:02:04.250 H:M:S, 9.0 percent\n"
            ),
            Some(3_724_250_000)
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
        // No proof is stored, so the data-path hash stays empty. The tx-path
        // hash is the placement data root.
        ChunkPathHashes {
            data_path_hash: None,
            tx_path_hash: Some(H256::repeat_byte(byte.wrapping_add(1))),
        }
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
}
