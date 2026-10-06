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
//! Large data-path and tx-path values go to blob files so compaction does
//! not rewrite them. Small offset rows stay in SST blocks. rust-rocksdb
//! 0.24 has no `set_allow_fallocate`; WAL recycle and `wal_bytes_per_sync`
//! are the durability knobs this binding exposes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eyre::WrapErr as _;
use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, H256, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::table::{Compress, Decode, Decompress, Encode, Table};
use rocksdb::{
    BlockBasedOptions, DB, DBCompactionStyle, DBCompressionType, Direction, IteratorMode, Options,
    WriteBatch, WriteOptions,
};

use super::group::{EnableStep, EngineLifetime, GroupCommit};
use super::interval::{IntervalRow, combine_hashes, coverage_gaps, plan_coverage};
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
/// Shared by every column family of this one database.
pub const BLOCK_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Values at or above this size move into blob files on the path column families.
pub const BLOB_MIN_BYTES: u64 = 2048;
/// Compaction threads for one HDD. The bench can select another preset.
const BACKGROUND_JOBS: i32 = 2;
/// Leveled SST target. Compaction output preallocation uses this as a cap.
const TARGET_FILE_BYTES: u64 = 256 * 1024 * 1024;
const SCHEMA_FILE: &str = "SCHEMA";
const SCHEMA_TEXT: &str = SUBMODULE_SCHEMA;

/// One RocksDB open. Production uses [`RocksTuning::baseline`].
///
/// The bench names the other values. Each of those changes one field, so a
/// sweep can attribute a difference to that field. Options are fixed at
/// open: a new block size needs an empty directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RocksTuning {
    pub name: &'static str,
    pub block_bytes: usize,
    pub block_cache_bytes: usize,
    /// `true` is LZ4 on SST and blob files. `false` stores both uncompressed.
    pub compress: bool,
    /// Blob files on the data-path and tx-path families.
    pub blobs: bool,
    /// Direct I/O for flush and compaction. Reads stay buffered.
    pub direct_io: bool,
    pub background_jobs: i32,
    pub target_file_bytes: u64,
    /// Universal compaction. `false` is leveled compaction.
    pub universal: bool,
}

impl RocksTuning {
    pub const fn baseline() -> Self {
        Self {
            name: "baseline",
            block_bytes: BLOCK_BYTES,
            block_cache_bytes: BLOCK_CACHE_BYTES,
            compress: true,
            blobs: true,
            direct_io: false,
            background_jobs: BACKGROUND_JOBS,
            target_file_bytes: TARGET_FILE_BYTES,
            universal: false,
        }
    }

    /// Named opens. `baseline` matches [`Self::baseline`].
    pub const fn presets() -> [Self; 8] {
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
                ..base
            },
            Self {
                name: "no-blob",
                blobs: false,
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
    pub fn with_block_cache(mut self, bytes: usize) -> Self {
        self.block_cache_bytes = bytes;
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
        format!(
            "rocks={} compression={compression} block_bytes={} blob={blob} direct_io={direct_io} compaction={compaction} jobs={} target_file_bytes={} block_cache_bytes={}",
            self.name,
            self.block_bytes,
            self.background_jobs,
            self.target_file_bytes,
            self.block_cache_bytes,
        )
    }

    /// Knobs only. The name is not a knob.
    #[cfg(test)]
    fn knob_diffs(self, other: Self) -> usize {
        usize::from(self.block_bytes != other.block_bytes)
            + usize::from(self.block_cache_bytes != other.block_cache_bytes)
            + usize::from(self.compress != other.compress)
            + usize::from(self.blobs != other.blobs)
            + usize::from(self.direct_io != other.direct_io)
            + usize::from(self.background_jobs != other.background_jobs)
            + usize::from(self.target_file_bytes != other.target_file_bytes)
            + usize::from(self.universal != other.universal)
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

    fn uses_blobs(self) -> bool {
        matches!(self, Self::DataPath | Self::TxPath)
    }
}

#[derive(Clone)]
pub struct RocksSubmoduleStore {
    /// Dropped first so the delay thread joins before `db` closes.
    lifetime: Arc<EngineLifetime>,
    db: Arc<DB>,
    write: Arc<Mutex<()>>,
    group: Arc<GroupCommit>,
    path: PathBuf,
    /// Statistics object from the `Options` that opened this DB.
    ///
    /// The DB copies that shared pointer. Ticker reads have to use this
    /// object, so the bench keeps it. Production open leaves it empty.
    /// Dump and persist periods are zero: a periodic stats write would show
    /// up as disk writes during a read sample.
    stats: Option<Arc<Options>>,
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
        Self::open_with(path, RocksTuning::baseline())
    }

    /// Same open as [`Self::open`], with an explicit block cache.
    ///
    /// The bench uses this to repeat a read test at more than one cache size.
    /// Production keeps [`BLOCK_CACHE_BYTES`].
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
        Self::open_inner(path, tuning, false, false)
    }

    /// Bench open. Same options as [`Self::open_with`], plus ticker counters
    /// and a full table preload.
    ///
    /// Histograms and timers stay off. `max_open_files` is -1, so `DB::Open`
    /// preloads every table on 16 threads. Production [`Self::open`] does not
    /// call this and stays at 512 until a rerun shows the read tail moved
    /// into open time.
    pub fn open_with_stats(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Self::open_inner(path, tuning, true, true)
    }

    fn open_inner(
        path: impl AsRef<Path>,
        tuning: RocksTuning,
        collect_stats: bool,
        preload_tables: bool,
    ) -> eyre::Result<Self> {
        let path = path.as_ref().to_path_buf();
        check_schema_file(&path)?;
        let (db, stats) = open_db(&path, &tuning, collect_stats, preload_tables)?;
        ensure_schema_row(&db)?;
        let marker = path.join(SCHEMA_FILE);
        if !marker.exists() {
            fs::write(&marker, SCHEMA_TEXT).wrap_err("write schema marker")?;
        }
        let group = GroupCommit::new();
        Ok(Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            db: Arc::new(db),
            write: Arc::new(Mutex::new(())),
            group,
            path,
            stats,
        })
    }

    pub(super) fn enable_group_commit(&self, txs_per_sync: u32) -> eyre::Result<()> {
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
        let _guard = self.lock_write();
        sync_wal_locked(&self.db, &self.group)
    }

    /// Visible registration. The caller holds no write lock.
    pub(super) fn update_registration<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        if !self.group.is_enabled() {
            return self.update(f);
        }
        let _guard = self.lock_write();
        let result = self.apply(false, f)?;
        if self.group.note_visible() {
            sync_wal_locked(&self.db, &self.group)?;
        }
        Ok(result)
    }

    fn spawn_delay(&self) -> eyre::Result<()> {
        let db = Arc::downgrade(&self.db);
        let write = Arc::clone(&self.write);
        let group = Arc::clone(&self.group);
        self.lifetime
            .spawn(Arc::clone(&self.group), move || match db.upgrade() {
                Some(db) => {
                    let _guard = write
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sync_wal_locked(&db, &group)
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
        batch.commit(sync)?;
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
        // The synced WAL covers any open registration group.
        self.group.cover_durable();
        Ok(())
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
}

impl super::SubmoduleStore for RocksSubmoduleStore {
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
        let _guard = self.lock_write();
        let result = self.apply(true, f)?;
        // A synced write covers earlier unsynced registrations in this WAL.
        self.group.cover_durable();
        Ok(result)
    }
}

fn sync_wal_locked(db: &DB, group: &GroupCommit) -> eyre::Result<()> {
    let Some(generation) = group.dirty_generation() else {
        return Ok(());
    };
    db.flush_wal(true).wrap_err("sync submodule index wal")?;
    group.mark_synced(generation, true);
    Ok(())
}

fn open_db(
    path: &Path,
    tuning: &RocksTuning,
    collect_stats: bool,
    preload_tables: bool,
) -> eyre::Result<(DB, Option<Arc<Options>>)> {
    let cache = rocksdb::Cache::new_lru_cache(tuning.block_cache_bytes);
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    // One or two background jobs so compaction does not flood an HDD.
    db_opts.set_max_background_jobs(tuning.background_jobs);
    db_opts.set_bytes_per_sync(1024 * 1024);
    db_opts.set_wal_bytes_per_sync(1024 * 1024);
    db_opts.set_recycle_log_file_num(4);
    if preload_tables {
        // Bench only. -1 keeps every table open and preloads them in DB::Open.
        // Production stays at 512 until a rerun shows the read tail moved there.
        db_opts.set_max_open_files(-1);
        db_opts.set_max_file_opening_threads(16);
        // Skip table properties that only choose compaction inputs.
        db_opts.set_skip_stats_update_on_db_open(true);
    } else {
        db_opts.set_max_open_files(512);
    }
    db_opts.set_use_fsync(true);
    if collect_stats {
        // Tickers only. Timers and histograms stay off.
        db_opts.enable_statistics();
        db_opts.set_statistics_level(rocksdb::statistics::StatsLevel::ExceptHistogramOrTimers);
        db_opts.set_stats_dump_period_sec(0);
        db_opts.set_stats_persist_period_sec(0);
    }

    let families = Cf::ALL.into_iter().map(|cf| {
        rocksdb::ColumnFamilyDescriptor::new(cf.name(), column_options(&cache, cf, tuning))
    });
    let db = DB::open_cf_descriptors(&db_opts, path, families)
        .wrap_err("open rocksdb submodule index")?;
    let stats = collect_stats.then(|| Arc::new(db_opts));
    Ok((db, stats))
}

fn property_u64(db: &DB, name: impl rocksdb::CStrLike) -> eyre::Result<u64> {
    db.property_int_value(name)
        .wrap_err("read rocksdb property")?
        .ok_or_else(|| eyre::eyre!("rocksdb property missing"))
}

fn column_options(cache: &rocksdb::Cache, cf: Cf, tuning: &RocksTuning) -> Options {
    let mut table = BlockBasedOptions::default();
    table.set_block_size(tuning.block_bytes);
    // Full-filter bloom, ~10 bits per key. `false` selects the full filter.
    table.set_bloom_filter(10.0, false);
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
    if tuning.blobs && cf.uses_blobs() {
        opts.set_enable_blob_files(true);
        opts.set_min_blob_size(BLOB_MIN_BYTES);
        opts.set_blob_file_size(256 * 1024 * 1024);
        opts.set_blob_compression_type(compression);
        opts.set_enable_blob_gc(true);
        opts.set_blob_compaction_readahead_size(2 * 1024 * 1024);
        opts.set_blob_cache(cache);
    }
    opts
}

fn check_schema_file(path: &Path) -> eyre::Result<()> {
    let marker = path.join(SCHEMA_FILE);
    if !marker.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&marker).wrap_err("read schema marker")?;
    if text != SCHEMA_TEXT {
        eyre::bail!(
            "schema file {} does not match this index engine",
            marker.display()
        );
    }
    Ok(())
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

fn cf_handle(db: &DB, cf: Cf) -> eyre::Result<&rocksdb::ColumnFamily> {
    db.cf_handle(cf.name())
        .ok_or_else(|| eyre::eyre!("missing column family {}", cf.name()))
}

fn encode_key<K: Encode>(key: K) -> Vec<u8> {
    key.encode().as_ref().to_vec()
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
        for key in edit.delete {
            self.delete(Cf::Interval, &encode_key(key));
        }
        for row in edit.put {
            self.put(
                Cf::Interval,
                &encode_key(row.start),
                &TxPathInterval {
                    end: row.end,
                    tx_path_hash: row.tx_path_hash,
                },
            );
        }
        Ok(())
    }

    fn delete_offset_range(
        &mut self,
        cf: Cf,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let start_key = encode_key(start);
        let keys = {
            let rows = self.rows();
            let mut cursor = rows.cursor(cf, Some(&start_key))?;
            let mut keys = Vec::new();
            while let Some((key, _)) = cursor.next_kv()? {
                let offset = PartitionChunkOffset::decode(&key)?;
                if offset > end {
                    break;
                }
                keys.push(key);
            }
            keys
        };
        for key in keys {
            self.delete(cf, &key);
        }
        Ok(())
    }

    fn commit(&self, sync: bool) -> eyre::Result<()> {
        let mut batch = WriteBatch::new();
        for cf in Cf::ALL {
            let handle = cf_handle(self.db, cf)?;
            if self.overlay.cleared[cf.index()] {
                for item in self.snap.iterator_cf(&handle, IteratorMode::Start) {
                    let (key, _) = item.wrap_err("scan column family for clear")?;
                    batch.delete_cf(&handle, key);
                }
            }
            for (key, value) in &self.overlay.maps[cf.index()] {
                match value {
                    Some(value) => batch.put_cf(&handle, key, value),
                    None => batch.delete_cf(&handle, key),
                }
            }
        }
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
            if overlay.cleared[cf.index()] {
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
        let db = self.snap.iterator_cf(&handle, mode).map(|item| {
            item.map(|(key, value)| (key.into_vec(), value.into_vec()))
                .map_err(|err| eyre::eyre!("{err}"))
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
        let mut db_next = match db_iter.as_mut().and_then(|iter| iter.next()) {
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
            .and_then(|iter| iter.next())
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
                    .and_then(|iter| iter.next())
                    .map(|(next_key, next_value)| (next_key.clone(), next_value.clone()));
                if db_key.as_deref() == Some(found_key.as_slice()) {
                    db_next = match db_iter.as_mut().and_then(|iter| iter.next()) {
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
            } else {
                return Ok(db_next);
            }
        }
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
        let from = match floor.as_ref() {
            Some((key, _)) if key.as_slice() <= start_key.as_slice() => Some(key.as_slice()),
            _ => Some(start_key.as_slice()),
        };
        let mut cursor = self.cursor(Cf::Interval, from)?;
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
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        let start_key = start.map(encode_key);
        self.collect_until(Cf::Pending, start_key.as_deref(), |_| false)
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
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        self.rows().pending_body_migrations_from(start)
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
        self.put(Cf::DataRoots, &encode_key(data_root), &infos);
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
        let offsets: Vec<PartitionChunkOffset> = self
            .rows()
            .pending_body_migrations_from(Some(start))?
            .into_iter()
            .map(|(offset, _)| offset)
            .take_while(|offset| *offset <= end)
            .collect();
        let removed = offsets.len();
        for offset in offsets {
            self.delete(Cf::Pending, &encode_key(offset));
        }
        Ok(removed)
    }

    fn clear(&mut self) -> eyre::Result<()> {
        for cf in Cf::INDEX {
            self.overlay.cleared[cf.index()] = true;
            self.overlay.maps[cf.index()].clear();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submodule::SubmoduleIndex;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::H256;

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

    #[test]
    fn presets_change_one_knob_and_open() -> eyre::Result<()> {
        let base = RocksTuning::baseline();
        assert_eq!(base, RocksTuning::preset("baseline")?);
        assert_eq!(base.block_bytes, BLOCK_BYTES);
        assert_eq!(base.block_cache_bytes, BLOCK_CACHE_BYTES);
        assert!(base.compress && base.blobs && !base.direct_io && !base.universal);
        assert_eq!(base.background_jobs, BACKGROUND_JOBS);
        assert_eq!(base.target_file_bytes, TARGET_FILE_BYTES);

        let block_16k = RocksTuning::preset("block-16k")?;
        let block_256k = RocksTuning::preset("block-256k")?;
        let cache = RocksTuning::preset("cache-1g")?;
        assert_eq!(block_16k.block_bytes, 16 * 1024);
        assert_eq!(block_256k.block_bytes, 256 * 1024);
        assert_eq!(cache.block_cache_bytes, 1 << 30);
        assert!(!RocksTuning::preset("no-blob")?.blobs);
        assert!(!RocksTuning::preset("no-compress")?.compress);
        assert!(RocksTuning::preset("direct-io")?.direct_io);
        assert!(RocksTuning::preset("universal")?.universal);

        let mut names = Vec::new();
        for preset in RocksTuning::presets() {
            assert!(!names.contains(&preset.name), "duplicate {}", preset.name);
            names.push(preset.name);
            if preset.name == "baseline" {
                assert_eq!(preset.knob_diffs(base), 0);
            } else {
                assert_eq!(preset.knob_diffs(base), 1, "{}", preset.name);
            }
        }
        assert!(RocksTuning::preset("missing").is_err());

        let overridden = cache.with_block_cache(4096);
        assert_eq!(overridden.name, "cache-1g");
        assert_eq!(overridden.block_cache_bytes, 4096);
        assert_eq!(overridden.knob_diffs(base), 1);

        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_presets")
            .build();
        for preset in RocksTuning::presets() {
            let path = dir.path().join(preset.name);
            let store = RocksSubmoduleStore::open_with(&path, preset)?;
            drop(store);
        }
        Ok(())
    }

    #[test]
    fn bench_open_counts_file_opens_and_production_open_does_not() -> eyre::Result<()> {
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
        assert!(plain.background()?.no_file_opens.is_none());
        let plain_opts = persisted_options(&dir.path().join("plain"))?;
        assert!(plain_opts.contains("max_open_files=512"), "{plain_opts}");
        assert!(!plain_opts.contains("max_open_files=-1"), "{plain_opts}");
        Ok(())
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
