//! Offline copy between the MDBX and RocksDB submodule indexes.
//!
//! The node must be stopped. This tool takes the engine lock (MDBX exclusive
//! open, RocksDB `LOCK`) and refuses the run when that lock is already held.
//! It does not require the directory name to contain `copy`.
//!
//! The source is not modified, except a schema v1 MDBX directory, which is
//! migrated to v2 before the copy. The completion marker is written last:
//! the RocksDB `SCHEMA` file, or the MDBX schema row [`SUBMODULE_SCHEMA`].
//! A crash leaves no completion marker. Delete the target and run again
//! with `--replace-incomplete`.
//!
//! Rows are read in bounded batches and written through the store write
//! path, so each engine uses its own encoding. Each batch commit is durable.
//! Verification reads both engines through the store read path.

use std::fs;
use std::path::Path;
use std::time::Instant;

use eyre::WrapErr as _;
use irys_types::{ChunkDataPath, DataRoot, PartitionChunkOffset, TxPath, TxPathHash};
use reth_db::mdbx::DatabaseArguments;
use reth_db::table::Table as _;

use super::rocks::RocksSubmoduleStore;
use super::store::{MdbxSubmoduleStore, SubmoduleStore};
use super::tables::{
    ChunkDataPathByOffset, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfos,
    DataRootInfosByDataRoot, PendingBodyMigration, PendingBodyMigrationsByOffset,
    SUBMODULE_CONVERT_INCOMPLETE, SUBMODULE_SCHEMA, TxLeafBinding, TxLeafBindingByTxPathHash,
    TxPathByTxPathHash, TxPathInterval, TxPathIntervalByStart,
};

const VERIFY_KEYS: usize = 256;
const SAMPLE_LIMIT: usize = 20;

/// One differing read. `at` is the offset or the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineMismatch {
    pub kind: &'static str,
    pub at: String,
}

/// Counts from one streaming compare of the two engines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineEquivalenceReport {
    pub offsets_checked: u64,
    pub offset_mismatches: u64,
    pub mismatch_data_path: u64,
    pub mismatch_tx_path: u64,
    pub mismatch_data_path_hash: u64,
    pub mismatch_tx_path_hash: u64,
    pub intervals_checked: u64,
    pub intervals_mismatches: u64,
    pub tx_paths_checked: u64,
    pub tx_paths_mismatches: u64,
    pub tx_leaf_bindings_checked: u64,
    pub tx_leaf_bindings_mismatches: u64,
    pub data_roots_checked: u64,
    pub data_roots_mismatches: u64,
    pub pending_checked: u64,
    pub pending_mismatches: u64,
    pub reset_marker_mismatch: u64,
    pub schema_mismatch: u64,
    pub samples: Vec<EngineMismatch>,
}

impl EngineEquivalenceReport {
    pub fn has_mismatch(&self) -> bool {
        self.offset_mismatches > 0
            || self.intervals_mismatches > 0
            || self.tx_paths_mismatches > 0
            || self.tx_leaf_bindings_mismatches > 0
            || self.data_roots_mismatches > 0
            || self.pending_mismatches > 0
            || self.reset_marker_mismatch > 0
            || self.schema_mismatch > 0
    }
}

impl std::fmt::Display for EngineEquivalenceReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "offsets_checked: {}", self.offsets_checked)?;
        writeln!(f, "offset_mismatches: {}", self.offset_mismatches)?;
        writeln!(f, "mismatch_data_path: {}", self.mismatch_data_path)?;
        writeln!(f, "mismatch_tx_path: {}", self.mismatch_tx_path)?;
        writeln!(
            f,
            "mismatch_data_path_hash: {}",
            self.mismatch_data_path_hash
        )?;
        writeln!(f, "mismatch_tx_path_hash: {}", self.mismatch_tx_path_hash)?;
        writeln!(f, "intervals_checked: {}", self.intervals_checked)?;
        writeln!(f, "intervals_mismatches: {}", self.intervals_mismatches)?;
        writeln!(f, "TxPathByTxPathHash_checked: {}", self.tx_paths_checked)?;
        writeln!(
            f,
            "TxPathByTxPathHash_mismatches: {}",
            self.tx_paths_mismatches
        )?;
        writeln!(
            f,
            "TxLeafBindingByTxPathHash_checked: {}",
            self.tx_leaf_bindings_checked
        )?;
        writeln!(
            f,
            "TxLeafBindingByTxPathHash_mismatches: {}",
            self.tx_leaf_bindings_mismatches
        )?;
        writeln!(
            f,
            "DataRootInfosByDataRoot_checked: {}",
            self.data_roots_checked
        )?;
        writeln!(
            f,
            "DataRootInfosByDataRoot_mismatches: {}",
            self.data_roots_mismatches
        )?;
        writeln!(
            f,
            "PendingBodyMigrationsByOffset_checked: {}",
            self.pending_checked
        )?;
        writeln!(
            f,
            "PendingBodyMigrationsByOffset_mismatches: {}",
            self.pending_mismatches
        )?;
        writeln!(f, "reset_marker_mismatch: {}", self.reset_marker_mismatch)?;
        writeln!(f, "schema_mismatch: {}", self.schema_mismatch)?;
        for sample in &self.samples {
            writeln!(f, "mismatch: {} {}", sample.kind, sample.at)?;
        }
        Ok(())
    }
}

/// Rows copied into the target. The completion marker is not a row count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConvertStats {
    pub migrated_v1: bool,
    pub tx_paths: u64,
    pub tx_leaf_bindings: u64,
    pub intervals: u64,
    pub path_hashes: u64,
    pub data_paths: u64,
    pub data_roots: u64,
    pub pending: u64,
    pub reset_marker: bool,
    pub elapsed_ms: u64,
}

impl std::fmt::Display for ConvertStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "migrated_v1: {}", self.migrated_v1)?;
        writeln!(f, "tx_paths: {}", self.tx_paths)?;
        writeln!(f, "tx_leaf_bindings: {}", self.tx_leaf_bindings)?;
        writeln!(f, "intervals: {}", self.intervals)?;
        writeln!(f, "path_hashes: {}", self.path_hashes)?;
        writeln!(f, "data_paths: {}", self.data_paths)?;
        writeln!(f, "data_roots: {}", self.data_roots)?;
        writeln!(f, "pending: {}", self.pending)?;
        writeln!(f, "reset_marker: {}", self.reset_marker)?;
        writeln!(f, "elapsed_ms: {}", self.elapsed_ms)?;
        Ok(())
    }
}

/// Knobs for one conversion. [`Self::fail_after_batches`] is a test hook.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub batch_rows: usize,
    pub batch_value_bytes: usize,
    pub replace_incomplete: bool,
    pub mdbx: DatabaseArguments,
    /// `Some(n)` returns an error after `n` committed row batches and does
    /// not write the completion marker. `Some(0)` returns before the first
    /// row batch. The target is left incomplete.
    pub fail_after_batches: Option<u32>,
}

impl ConvertOptions {
    /// Production batch size. `mdbx` should be the submodule arguments.
    /// The converter sets the exclusive lock itself.
    pub fn new(mdbx: DatabaseArguments) -> Self {
        Self {
            batch_rows: 2048,
            batch_value_bytes: 8 * 1024 * 1024,
            replace_incomplete: false,
            mdbx,
            fail_after_batches: None,
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    rows: usize,
    value_bytes: usize,
}

impl Limits {
    fn from_options(options: &ConvertOptions) -> Self {
        Self {
            rows: options.batch_rows.max(1),
            value_bytes: options.batch_value_bytes.max(1),
        }
    }
}

struct CopyProgress {
    batches: u32,
    fail_after_batches: Option<u32>,
}

impl CopyProgress {
    fn note(&mut self) -> eyre::Result<()> {
        self.batches = self.batches.saturating_add(1);
        if self.fail_after_batches == Some(self.batches) {
            eyre::bail!(
                "injected conversion interrupt after {} batches",
                self.batches
            );
        }
        Ok(())
    }
}

trait RowSource {
    fn read_tx_paths(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxPath)>>;

    fn read_leaves(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxLeafBinding)>>;

    fn read_intervals(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, TxPathInterval)>>;

    fn read_path_hashes(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>>;

    fn read_data_paths(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkDataPath)>>;

    fn read_data_roots(
        &self,
        after: Option<DataRoot>,
        limits: Limits,
    ) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>>;

    fn read_pending(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>>;

    fn read_offset_keys(
        &self,
        table: OffsetTable,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<PartitionChunkOffset>>;

    fn read_hash_keys(
        &self,
        table: HashTable,
        after: Option<TxPathHash>,
        limit: usize,
    ) -> eyre::Result<Vec<TxPathHash>>;
}

#[derive(Clone, Copy)]
enum OffsetTable {
    PathHashes,
    DataPaths,
    Pending,
}

#[derive(Clone, Copy)]
enum HashTable {
    TxPaths,
    Leaves,
    DataRoots,
}

impl RowSource for MdbxSubmoduleStore {
    fn read_tx_paths(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxPath)>> {
        self.read_rows::<TxPathByTxPathHash, _>(after, limits.rows, limits.value_bytes, |path| {
            path.len()
        })
    }

    fn read_leaves(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxLeafBinding)>> {
        self.read_rows::<TxLeafBindingByTxPathHash, _>(
            after,
            limits.rows,
            limits.value_bytes,
            |_| 64,
        )
    }

    fn read_intervals(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, TxPathInterval)>> {
        self.read_rows::<TxPathIntervalByStart, _>(after, limits.rows, limits.value_bytes, |_| 40)
    }

    fn read_path_hashes(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        self.read_rows::<ChunkPathHashesByOffset, _>(after, limits.rows, limits.value_bytes, |_| 64)
    }

    fn read_data_paths(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkDataPath)>> {
        self.read_rows::<ChunkDataPathByOffset, _>(after, limits.rows, limits.value_bytes, |path| {
            path.len()
        })
    }

    fn read_data_roots(
        &self,
        after: Option<DataRoot>,
        limits: Limits,
    ) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        self.read_rows::<DataRootInfosByDataRoot, _>(
            after,
            limits.rows,
            limits.value_bytes,
            |infos: &DataRootInfos| infos.0.len().saturating_mul(24),
        )
    }

    fn read_pending(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        self.read_rows::<PendingBodyMigrationsByOffset, _>(
            after,
            limits.rows,
            limits.value_bytes,
            |_| 80,
        )
    }

    fn read_offset_keys(
        &self,
        table: OffsetTable,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        match table {
            OffsetTable::PathHashes => self.read_keys::<ChunkPathHashesByOffset>(after, limit),
            OffsetTable::DataPaths => self.read_keys::<ChunkDataPathByOffset>(after, limit),
            OffsetTable::Pending => self.read_keys::<PendingBodyMigrationsByOffset>(after, limit),
        }
    }

    fn read_hash_keys(
        &self,
        table: HashTable,
        after: Option<TxPathHash>,
        limit: usize,
    ) -> eyre::Result<Vec<TxPathHash>> {
        match table {
            HashTable::TxPaths => self.read_keys::<TxPathByTxPathHash>(after, limit),
            HashTable::Leaves => self.read_keys::<TxLeafBindingByTxPathHash>(after, limit),
            HashTable::DataRoots => self.read_keys::<DataRootInfosByDataRoot>(after, limit),
        }
    }
}

impl RowSource for RocksSubmoduleStore {
    fn read_tx_paths(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxPath)>> {
        self.read_cf_batch(
            TxPathByTxPathHash::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |path: &TxPath| path.len(),
        )
    }

    fn read_leaves(
        &self,
        after: Option<TxPathHash>,
        limits: Limits,
    ) -> eyre::Result<Vec<(TxPathHash, TxLeafBinding)>> {
        self.read_cf_batch(
            TxLeafBindingByTxPathHash::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |_| 64,
        )
    }

    fn read_intervals(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, TxPathInterval)>> {
        self.read_cf_batch(
            TxPathIntervalByStart::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |_| 40,
        )
    }

    fn read_path_hashes(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
        self.read_cf_batch(
            ChunkPathHashesByOffset::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |_| 64,
        )
    }

    fn read_data_paths(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkDataPath)>> {
        self.read_cf_batch(
            ChunkDataPathByOffset::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |path: &ChunkDataPath| path.len(),
        )
    }

    fn read_data_roots(
        &self,
        after: Option<DataRoot>,
        limits: Limits,
    ) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
        self.read_cf_batch(
            DataRootInfosByDataRoot::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |infos: &DataRootInfos| infos.0.len().saturating_mul(24),
        )
    }

    fn read_pending(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        self.read_cf_batch(
            PendingBodyMigrationsByOffset::NAME,
            after,
            limits.rows,
            limits.value_bytes,
            |_| 80,
        )
    }

    fn read_offset_keys(
        &self,
        table: OffsetTable,
        after: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let name = match table {
            OffsetTable::PathHashes => ChunkPathHashesByOffset::NAME,
            OffsetTable::DataPaths => ChunkDataPathByOffset::NAME,
            OffsetTable::Pending => PendingBodyMigrationsByOffset::NAME,
        };
        self.read_cf_keys(name, after, limit)
    }

    fn read_hash_keys(
        &self,
        table: HashTable,
        after: Option<TxPathHash>,
        limit: usize,
    ) -> eyre::Result<Vec<TxPathHash>> {
        let name = match table {
            HashTable::TxPaths => TxPathByTxPathHash::NAME,
            HashTable::Leaves => TxLeafBindingByTxPathHash::NAME,
            HashTable::DataRoots => DataRootInfosByDataRoot::NAME,
        };
        self.read_cf_keys(name, after, limit)
    }
}

/// Copies a v2 MDBX submodule index into a new RocksDB directory.
///
/// A v1 source is migrated in place first. The RocksDB `SCHEMA` file is
/// written only after the rows are durable.
pub fn convert_mdbx_to_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    precheck(mdbx_dir, rocks_dir)?;
    if !mdbx_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "source {} has no mdbx.dat. Pass the submodule db/ directory.",
            mdbx_dir.display()
        );
    }
    prepare_rocks_target(rocks_dir, options.replace_incomplete)?;
    let args = exclusive_args(options);
    let (src, migration) = map_open(MdbxSubmoduleStore::open_with_stats(mdbx_dir, args))?;
    let dst = map_open(RocksSubmoduleStore::open_converting(rocks_dir))?;
    let mut stats = copy_index(&src, &dst, options)?;
    stats.migrated_v1 = migration.migrated;
    if src.reset_in_progress()? {
        dst.set_reset_in_progress()?;
        stats.reset_marker = true;
    }
    dst.stamp_schema_marker()?;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(stats)
}

/// Copies a completed RocksDB submodule index into a new MDBX directory.
///
/// The MDBX schema row stays [`SUBMODULE_CONVERT_INCOMPLETE`] until every
/// row is durable, then becomes [`SUBMODULE_SCHEMA`].
pub fn convert_rocks_to_mdbx(
    rocks_dir: &Path,
    mdbx_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    precheck(rocks_dir, mdbx_dir)?;
    if !rocks_marker_complete(rocks_dir)? {
        eyre::bail!(
            "source {} has no SCHEMA completion marker. Refusing to read an incomplete RocksDB index.",
            rocks_dir.display()
        );
    }
    let args = exclusive_args(options);
    prepare_mdbx_target(mdbx_dir, args.clone(), options.replace_incomplete)?;
    let src = map_open(RocksSubmoduleStore::open(rocks_dir))?;
    let dst = map_open(MdbxSubmoduleStore::open_for_convert(mdbx_dir, args))?;
    let mut stats = copy_index(&src, &dst, options)?;
    if src.reset_in_progress()? {
        dst.set_reset_in_progress()?;
        stats.reset_marker = true;
    }
    dst.put_schema_bytes(SUBMODULE_SCHEMA.as_bytes())?;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(stats)
}

/// Compares every row through the public read functions. Exit the process
/// when [`EngineEquivalenceReport::has_mismatch`] is set.
///
/// Does not migrate and does not write. A v1 MDBX directory is refused.
/// Run `mdbx-to-rocks` first; that command migrates v1.
pub fn verify_mdbx_and_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<EngineEquivalenceReport> {
    precheck(mdbx_dir, rocks_dir)?;
    let args = exclusive_args(options);
    require_completed_mdbx(mdbx_dir, args.clone())?;
    if !rocks_marker_complete(rocks_dir)? {
        eyre::bail!(
            "rocks index {} has no SCHEMA completion marker",
            rocks_dir.display()
        );
    }
    let mdbx = map_open(MdbxSubmoduleStore::open_with_stats(mdbx_dir, args))?.0;
    let rocks = map_open(RocksSubmoduleStore::open(rocks_dir))?;
    verify_open_stores(&mdbx, &rocks)
}

fn copy_index<S, D>(src: &S, dst: &D, options: &ConvertOptions) -> eyre::Result<ConvertStats>
where
    S: RowSource,
    D: SubmoduleStore,
{
    let mut progress = CopyProgress {
        batches: 0,
        fail_after_batches: options.fail_after_batches,
    };
    if progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limits = Limits::from_options(options);
    let stats = ConvertStats {
        tx_paths: copy_rows(
            |after| src.read_tx_paths(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (hash, path) in batch {
                        tx.add_full_tx_path(hash, path)?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        tx_leaf_bindings: copy_rows(
            |after| src.read_leaves(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (hash, binding) in &batch {
                        tx.add_tx_leaf_binding(*hash, binding)?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        intervals: copy_rows(
            |after| src.read_intervals(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (start, interval) in batch {
                        tx.add_tx_path_hash_to_offset_range(
                            start,
                            interval.end,
                            Some(interval.tx_path_hash),
                        )?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        path_hashes: copy_rows(
            |after| src.read_path_hashes(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (offset, hashes) in batch {
                        if hashes.data_path_hash.is_some() {
                            tx.add_data_path_hash_to_offset_index(offset, hashes.data_path_hash)?;
                        }
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        data_paths: copy_rows(
            |after| src.read_data_paths(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (offset, path) in batch {
                        tx.add_full_data_path(offset, path)?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        data_roots: copy_rows(
            |after| src.read_data_roots(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (root, infos) in batch {
                        tx.set_data_root_infos_for_data_root(root, infos)?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        pending: copy_rows(
            |after| src.read_pending(after, limits),
            |batch| {
                dst.update(|tx| {
                    for (offset, job) in &batch {
                        tx.add_pending_body_migration(*offset, job)?;
                    }
                    Ok(())
                })
            },
            &mut progress,
        )?,
        ..Default::default()
    };
    Ok(stats)
}

fn copy_rows<K, V>(
    mut read: impl FnMut(Option<K>) -> eyre::Result<Vec<(K, V)>>,
    mut write: impl FnMut(Vec<(K, V)>) -> eyre::Result<()>,
    progress: &mut CopyProgress,
) -> eyre::Result<u64>
where
    K: Copy,
{
    let mut after = None;
    let mut copied = 0_u64;
    loop {
        let batch = read(after)?;
        // `last` is Copy. Keep it before `write` moves the batch.
        let Some(last) = batch.last().map(|(key, _)| *key) else {
            break;
        };
        copied = copied.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        write(batch)?;
        progress.note()?;
        after = Some(last);
    }
    Ok(copied)
}

fn verify_open_stores(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
) -> eyre::Result<EngineEquivalenceReport> {
    let mut report = EngineEquivalenceReport::default();
    verify_offsets(mdbx, rocks, &mut report)?;
    verify_intervals(mdbx, rocks, &mut report)?;
    verify_hash_table(mdbx, rocks, HashTable::TxPaths, &mut report)?;
    verify_hash_table(mdbx, rocks, HashTable::Leaves, &mut report)?;
    verify_hash_table(mdbx, rocks, HashTable::DataRoots, &mut report)?;
    verify_pending(mdbx, rocks, &mut report)?;
    verify_meta(mdbx, rocks, &mut report)?;
    Ok(report)
}

#[derive(PartialEq, Eq)]
struct OffsetRead {
    data_path: Option<ChunkDataPath>,
    tx_path: Option<TxPath>,
    data_path_hash: Option<irys_types::H256>,
    tx_path_hash: Option<irys_types::H256>,
}

fn read_offsets(
    store: &impl SubmoduleStore,
    keys: &[PartitionChunkOffset],
) -> eyre::Result<Vec<OffsetRead>> {
    store.view(|tx| {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let hashes = tx.get_path_hashes_by_offset(*key)?;
            out.push(OffsetRead {
                data_path: tx.get_data_path_by_offset(*key)?,
                tx_path: tx.get_tx_path_by_offset(*key)?,
                data_path_hash: hashes.as_ref().and_then(|row| row.data_path_hash),
                tx_path_hash: hashes.as_ref().and_then(|row| row.tx_path_hash),
            });
        }
        Ok(out)
    })
}

fn verify_offsets(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    merge_keys(
        &mut |after| union_offset_keys(mdbx, after),
        &mut |after| union_offset_keys(rocks, after),
        |keys| {
            let left = read_offsets(mdbx, keys)?;
            let right = read_offsets(rocks, keys)?;
            for (index, key) in keys.iter().enumerate() {
                report.offsets_checked = report.offsets_checked.saturating_add(1);
                let at = format!("offset {}", key.0);
                let mut same = true;
                if left[index].data_path != right[index].data_path {
                    same = false;
                    report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                    note(report, "data_path", at.clone());
                }
                if left[index].tx_path != right[index].tx_path {
                    same = false;
                    report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
                    note(report, "tx_path", at.clone());
                }
                if left[index].data_path_hash != right[index].data_path_hash {
                    same = false;
                    report.mismatch_data_path_hash =
                        report.mismatch_data_path_hash.saturating_add(1);
                    note(report, "data_path_hash", at.clone());
                }
                if left[index].tx_path_hash != right[index].tx_path_hash {
                    same = false;
                    report.mismatch_tx_path_hash = report.mismatch_tx_path_hash.saturating_add(1);
                    note(report, "tx_path_hash", at);
                }
                if !same {
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                }
            }
            Ok(())
        },
    )
}

/// Path-hash keys and data-path keys, in offset order, at most [`VERIFY_KEYS`].
///
/// One offset that has both rows is emitted once. A key past the cap stays
/// for the next batch because the cursor restarts after the last emitted key.
fn union_offset_keys(
    store: &impl RowSource,
    after: Option<PartitionChunkOffset>,
) -> eyre::Result<Vec<PartitionChunkOffset>> {
    let path = store.read_offset_keys(OffsetTable::PathHashes, after, VERIFY_KEYS)?;
    let data = store.read_offset_keys(OffsetTable::DataPaths, after, VERIFY_KEYS)?;
    let mut out = Vec::new();
    let mut path_index = 0;
    let mut data_index = 0;
    while out.len() < VERIFY_KEYS {
        let key = match (path.get(path_index).copied(), data.get(data_index).copied()) {
            (None, None) => break,
            (Some(key), None) => {
                path_index += 1;
                key
            }
            (None, Some(key)) => {
                data_index += 1;
                key
            }
            (Some(left), Some(right)) if left < right => {
                path_index += 1;
                left
            }
            (Some(left), Some(right)) if right < left => {
                data_index += 1;
                right
            }
            (Some(key), Some(_)) => {
                path_index += 1;
                data_index += 1;
                key
            }
        };
        out.push(key);
    }
    Ok(out)
}

fn verify_intervals(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let limits = Limits {
        rows: VERIFY_KEYS,
        value_bytes: usize::MAX,
    };
    merge_rows(
        &mut |after| mdbx.read_intervals(after, limits),
        &mut |after| rocks.read_intervals(after, limits),
        |rows| {
            let mut points = Vec::new();
            for (key, left, right) in rows {
                report.intervals_checked = report.intervals_checked.saturating_add(1);
                if left != right {
                    report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                    note(report, "interval", format!("offset {}", key.0));
                }
                points.push(*key);
                if let Some(row) = left.as_ref().or(right.as_ref()) {
                    points.push(row.end);
                }
            }
            points.sort_by_key(|offset| offset.0);
            points.dedup();
            let left = read_offsets(mdbx, &points)?;
            let right = read_offsets(rocks, &points)?;
            for (index, key) in points.iter().enumerate() {
                if left[index].tx_path != right[index].tx_path
                    || left[index].tx_path_hash != right[index].tx_path_hash
                {
                    report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                    note(report, "interval_read", format!("offset {}", key.0));
                }
            }
            Ok(())
        },
    )
}

fn verify_hash_table(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
    table: HashTable,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    merge_keys(
        &mut |after| mdbx.read_hash_keys(table, after, VERIFY_KEYS),
        &mut |after| rocks.read_hash_keys(table, after, VERIFY_KEYS),
        |keys| {
            let left = read_hash_values(mdbx, table, keys)?;
            let right = read_hash_values(rocks, table, keys)?;
            for (index, key) in keys.iter().enumerate() {
                let (checked, mismatches, kind) = hash_counters(table);
                *checked_mut(report, checked) = checked_of(report, checked).saturating_add(1);
                if left[index] != right[index] {
                    *mismatch_mut(report, mismatches) =
                        mismatch_of(report, mismatches).saturating_add(1);
                    note(report, kind, format!("{key:x}"));
                }
            }
            Ok(())
        },
    )
}

enum HashValue {
    TxPath(Option<TxPath>),
    Leaf(Option<TxLeafBinding>),
    Root(Option<DataRootInfos>),
}

impl PartialEq for HashValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::TxPath(left), Self::TxPath(right)) => left == right,
            (Self::Leaf(left), Self::Leaf(right)) => left == right,
            (Self::Root(left), Self::Root(right)) => left == right,
            _ => false,
        }
    }
}

fn read_hash_values(
    store: &impl SubmoduleStore,
    table: HashTable,
    keys: &[TxPathHash],
) -> eyre::Result<Vec<HashValue>> {
    store.view(|tx| {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let value = match table {
                HashTable::TxPaths => HashValue::TxPath(tx.get_full_tx_path(*key)?),
                HashTable::Leaves => HashValue::Leaf(tx.get_tx_leaf_binding(*key)?),
                HashTable::DataRoots => {
                    HashValue::Root(tx.get_data_root_infos_for_data_root(*key)?)
                }
            };
            out.push(value);
        }
        Ok(out)
    })
}

#[derive(Clone, Copy)]
enum Counter {
    TxPaths,
    Leaves,
    Roots,
}

fn hash_counters(table: HashTable) -> (Counter, Counter, &'static str) {
    match table {
        HashTable::TxPaths => (Counter::TxPaths, Counter::TxPaths, "TxPathByTxPathHash"),
        HashTable::Leaves => (
            Counter::Leaves,
            Counter::Leaves,
            "TxLeafBindingByTxPathHash",
        ),
        HashTable::DataRoots => (Counter::Roots, Counter::Roots, "DataRootInfosByDataRoot"),
    }
}

fn checked_of(report: &EngineEquivalenceReport, which: Counter) -> u64 {
    match which {
        Counter::TxPaths => report.tx_paths_checked,
        Counter::Leaves => report.tx_leaf_bindings_checked,
        Counter::Roots => report.data_roots_checked,
    }
}

fn checked_mut(report: &mut EngineEquivalenceReport, which: Counter) -> &mut u64 {
    match which {
        Counter::TxPaths => &mut report.tx_paths_checked,
        Counter::Leaves => &mut report.tx_leaf_bindings_checked,
        Counter::Roots => &mut report.data_roots_checked,
    }
}

fn mismatch_of(report: &EngineEquivalenceReport, which: Counter) -> u64 {
    match which {
        Counter::TxPaths => report.tx_paths_mismatches,
        Counter::Leaves => report.tx_leaf_bindings_mismatches,
        Counter::Roots => report.data_roots_mismatches,
    }
}

fn mismatch_mut(report: &mut EngineEquivalenceReport, which: Counter) -> &mut u64 {
    match which {
        Counter::TxPaths => &mut report.tx_paths_mismatches,
        Counter::Leaves => &mut report.tx_leaf_bindings_mismatches,
        Counter::Roots => &mut report.data_roots_mismatches,
    }
}

fn verify_pending(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    merge_keys(
        &mut |after| mdbx.read_offset_keys(OffsetTable::Pending, after, VERIFY_KEYS),
        &mut |after| rocks.read_offset_keys(OffsetTable::Pending, after, VERIFY_KEYS),
        |keys| {
            let left = read_pending(mdbx, keys)?;
            let right = read_pending(rocks, keys)?;
            for (index, key) in keys.iter().enumerate() {
                report.pending_checked = report.pending_checked.saturating_add(1);
                if left[index] != right[index] {
                    report.pending_mismatches = report.pending_mismatches.saturating_add(1);
                    note(
                        report,
                        "PendingBodyMigrationsByOffset",
                        format!("offset {}", key.0),
                    );
                }
            }
            Ok(())
        },
    )
}

fn read_pending(
    store: &impl SubmoduleStore,
    keys: &[PartitionChunkOffset],
) -> eyre::Result<Vec<Option<PendingBodyMigration>>> {
    store.view(|tx| {
        keys.iter()
            .map(|key| tx.get_pending_body_migration(*key))
            .collect()
    })
}

fn verify_meta(
    mdbx: &MdbxSubmoduleStore,
    rocks: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    if mdbx.reset_in_progress()? != rocks.reset_in_progress()? {
        report.reset_marker_mismatch = 1;
        note(report, "reset_marker", "metadata".to_owned());
    }
    let left = mdbx.schema_bytes()?;
    let right = rocks.schema_bytes()?;
    let expected = Some(SUBMODULE_SCHEMA.as_bytes().to_vec());
    if left != expected || right != expected || left != right {
        report.schema_mismatch = 1;
        note(report, "schema", "metadata".to_owned());
    }
    Ok(())
}

fn merge_keys<K, F>(
    next_left: &mut dyn FnMut(Option<K>) -> eyre::Result<Vec<K>>,
    next_right: &mut dyn FnMut(Option<K>) -> eyre::Result<Vec<K>>,
    mut on_batch: F,
) -> eyre::Result<()>
where
    K: Copy + Ord,
    F: FnMut(&[K]) -> eyre::Result<()>,
{
    let mut left = next_left(None)?;
    let mut right = next_right(None)?;
    let mut li = 0;
    let mut ri = 0;
    let mut left_done = left.is_empty();
    let mut right_done = right.is_empty();
    let mut pending = Vec::new();
    loop {
        if li >= left.len() && !left_done {
            let after = left.last().copied();
            left = next_left(after)?;
            li = 0;
            left_done = left.is_empty();
        }
        if ri >= right.len() && !right_done {
            let after = right.last().copied();
            right = next_right(after)?;
            ri = 0;
            right_done = right.is_empty();
        }
        let left_key = if left_done || li >= left.len() {
            None
        } else {
            Some(left[li])
        };
        let right_key = if right_done || ri >= right.len() {
            None
        } else {
            Some(right[ri])
        };
        let key = match (left_key, right_key) {
            (None, None) => break,
            (Some(key), None) => {
                li += 1;
                key
            }
            (None, Some(key)) => {
                ri += 1;
                key
            }
            (Some(left_key), Some(right_key)) if left_key < right_key => {
                li += 1;
                left_key
            }
            (Some(left_key), Some(right_key)) if right_key < left_key => {
                ri += 1;
                right_key
            }
            (Some(key), Some(_)) => {
                li += 1;
                ri += 1;
                key
            }
        };
        pending.push(key);
        if pending.len() == VERIFY_KEYS {
            on_batch(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        on_batch(&pending)?;
    }
    Ok(())
}

fn merge_rows<K, V, F>(
    next_left: &mut dyn FnMut(Option<K>) -> eyre::Result<Vec<(K, V)>>,
    next_right: &mut dyn FnMut(Option<K>) -> eyre::Result<Vec<(K, V)>>,
    mut on_batch: F,
) -> eyre::Result<()>
where
    K: Copy + Ord,
    V: Clone,
    F: FnMut(&[(K, Option<V>, Option<V>)]) -> eyre::Result<()>,
{
    let mut left = next_left(None)?;
    let mut right = next_right(None)?;
    let mut li = 0;
    let mut ri = 0;
    let mut left_done = left.is_empty();
    let mut right_done = right.is_empty();
    let mut pending = Vec::new();
    loop {
        if li >= left.len() && !left_done {
            let after = left.last().map(|(key, _)| *key);
            left = next_left(after)?;
            li = 0;
            left_done = left.is_empty();
        }
        if ri >= right.len() && !right_done {
            let after = right.last().map(|(key, _)| *key);
            right = next_right(after)?;
            ri = 0;
            right_done = right.is_empty();
        }
        let left_key = (!left_done && li < left.len()).then(|| left[li].0);
        let right_key = (!right_done && ri < right.len()).then(|| right[ri].0);
        let row = match (left_key, right_key) {
            (None, None) => break,
            (Some(_), None) => {
                let (key, value) = (left[li].0, left[li].1.clone());
                li += 1;
                (key, Some(value), None)
            }
            (None, Some(_)) => {
                let (key, value) = (right[ri].0, right[ri].1.clone());
                ri += 1;
                (key, None, Some(value))
            }
            (Some(left_key), Some(right_key)) if left_key < right_key => {
                let (key, value) = (left[li].0, left[li].1.clone());
                li += 1;
                (key, Some(value), None)
            }
            (Some(left_key), Some(right_key)) if right_key < left_key => {
                let (key, value) = (right[ri].0, right[ri].1.clone());
                ri += 1;
                (key, None, Some(value))
            }
            (Some(_), Some(_)) => {
                let (key, left_value) = (left[li].0, left[li].1.clone());
                let right_value = right[ri].1.clone();
                li += 1;
                ri += 1;
                (key, Some(left_value), Some(right_value))
            }
        };
        pending.push(row);
        if pending.len() == VERIFY_KEYS {
            on_batch(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        on_batch(&pending)?;
    }
    Ok(())
}

fn note(report: &mut EngineEquivalenceReport, kind: &'static str, at: String) {
    if report.samples.len() < SAMPLE_LIMIT {
        report.samples.push(EngineMismatch { kind, at });
    }
}

fn precheck(src: &Path, dst: &Path) -> eyre::Result<()> {
    if same_path(src, dst) {
        eyre::bail!("source and target are the same path ({})", src.display());
    }
    if !src.is_dir() {
        eyre::bail!("source {} is not a directory", src.display());
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn exclusive_args(options: &ConvertOptions) -> DatabaseArguments {
    options.mdbx.clone().with_exclusive(Some(true))
}

fn map_open<T>(result: eyre::Result<T>) -> eyre::Result<T> {
    result.map_err(map_lock)
}

fn map_lock(err: eyre::Report) -> eyre::Report {
    if is_lock_error(&err) {
        eyre::eyre!(
            "refusing to run because the index lock is held, so the node may be running. Stop the node and retry. {err}"
        )
    } else {
        err
    }
}

fn is_lock_error(err: &eyre::Report) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("busy")
        || text.contains("lock hold")
        || text.contains("lock file")
        || text.contains("resource temporarily unavailable")
        || text.contains("already in use")
        || text.contains("currently in use")
        || text.contains("another write transaction")
}

fn dir_is_empty(path: &Path) -> eyre::Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
}

fn rocks_marker_complete(path: &Path) -> eyre::Result<bool> {
    let marker = path.join("SCHEMA");
    if !marker.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(&marker).wrap_err("read SCHEMA")?;
    Ok(text == SUBMODULE_SCHEMA)
}

fn prepare_rocks_target(path: &Path, replace: bool) -> eyre::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        return Ok(());
    }
    if !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if dir_is_empty(path)? {
        return Ok(());
    }
    if rocks_marker_complete(path)? {
        eyre::bail!(
            "target {} is a completed RocksDB index. Refusing to replace it.",
            path.display()
        );
    }
    if !replace {
        eyre::bail!(
            "target {} is non-empty and has no SCHEMA completion marker. Pass --replace-incomplete to delete it and start over.",
            path.display()
        );
    }
    match RocksSubmoduleStore::open_converting(path) {
        Ok(store) => drop(store),
        Err(err) if is_lock_error(&err) => return Err(map_lock(err)),
        Err(_) => {}
    }
    fs::remove_dir_all(path).wrap_err("remove incomplete rocks target")?;
    fs::create_dir_all(path)?;
    Ok(())
}

fn prepare_mdbx_target(path: &Path, args: DatabaseArguments, replace: bool) -> eyre::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        return Ok(());
    }
    if !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if dir_is_empty(path)? {
        return Ok(());
    }
    if path.join("mdbx.dat").is_file() {
        match read_mdbx_schema(path, args) {
            Ok(Some(bytes)) if bytes.as_slice() == SUBMODULE_SCHEMA.as_bytes() => {
                eyre::bail!(
                    "target {} is a completed MDBX index. Refusing to replace it.",
                    path.display()
                );
            }
            Err(err) if is_lock_error(&err) => return Err(map_lock(err)),
            Ok(_) | Err(_) => {}
        }
    }
    if !replace {
        eyre::bail!(
            "target {} is non-empty and has no completion marker. Pass --replace-incomplete to delete it and start over.",
            path.display()
        );
    }
    fs::remove_dir_all(path).wrap_err("remove incomplete mdbx target")?;
    fs::create_dir_all(path)?;
    Ok(())
}

fn read_mdbx_schema(path: &Path, args: DatabaseArguments) -> eyre::Result<Option<Vec<u8>>> {
    use crate::db::IrysDatabaseExt as _;
    use reth_db::DatabaseEnv;
    use reth_db::DatabaseEnvKind;

    let env = DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .map_err(|err| eyre::eyre!("open {}: {err}", path.display()))?;
    env.view_eyre(super::db::read_schema_bytes)
}

fn require_completed_mdbx(path: &Path, args: DatabaseArguments) -> eyre::Result<()> {
    if !path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "MDBX index {} has no mdbx.dat. Pass the submodule db/ directory.",
            path.display()
        );
    }
    match read_mdbx_schema(path, args)? {
        Some(bytes) if bytes.as_slice() == SUBMODULE_SCHEMA.as_bytes() => Ok(()),
        Some(bytes) if bytes.as_slice() == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() => {
            eyre::bail!(
                "MDBX index {} is an incomplete conversion. Delete it and rerun `submodule-index-convert rocks-to-mdbx` with --replace-incomplete.",
                path.display()
            )
        }
        _ => eyre::bail!(
            "MDBX index {} is not a completed v2 index. Run `submodule-index-convert mdbx-to-rocks`, which migrates schema v1 before the copy.",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, PartitionChunkOffset, RelativeChunkOffset};
    use reth_db::mdbx::DatabaseArguments;

    use super::super::store::{MdbxSubmoduleStore, SubmoduleStore};
    use super::super::tables::{
        ChunkPathHashes, DataRootInfo, DataRootInfos, PendingBodyMigration, SUBMODULE_SCHEMA,
        TxLeafBinding,
    };
    use super::{
        ConvertOptions, convert_mdbx_to_rocks, convert_rocks_to_mdbx, verify_mdbx_and_rocks,
    };
    use crate::IrysDatabaseArgs as _;
    use crate::submodule::rocks::RocksSubmoduleStore;

    fn h(byte: u8) -> H256 {
        H256::repeat_byte(byte)
    }

    fn testing_args() -> eyre::Result<DatabaseArguments> {
        DatabaseArguments::irys_testing()
    }

    fn options() -> eyre::Result<ConvertOptions> {
        Ok(ConvertOptions::new(testing_args()?))
    }

    fn temp(prefix: &str) -> tempfile::TempDir {
        TempDirBuilder::new().prefix(prefix).build()
    }

    fn populate(store: &impl SubmoduleStore) -> eyre::Result<()> {
        store.update(|tx| {
            tx.add_full_tx_path(h(1), b"tx-path-1".to_vec())?;
            tx.add_full_tx_path(h(2), b"tx-path-2".to_vec())?;
            tx.add_tx_leaf_binding(
                h(1),
                &TxLeafBinding {
                    data_root: h(9),
                    prefix_hash: h(8),
                },
            )?;
            tx.add_tx_leaf_binding(
                h(2),
                &TxLeafBinding {
                    data_root: h(7),
                    prefix_hash: h(6),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(10),
                PartitionChunkOffset(12),
                Some(h(1)),
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(20),
                PartitionChunkOffset(20),
                Some(h(2)),
            )?;
            tx.add_full_data_path(PartitionChunkOffset(10), b"data-10".to_vec())?;
            tx.add_data_path_hash_to_offset_index(PartitionChunkOffset(10), Some(h(10)))?;
            tx.add_full_data_path(PartitionChunkOffset(11), b"data-11".to_vec())?;
            tx.add_data_path_hash_to_offset_index(PartitionChunkOffset(11), Some(h(11)))?;
            tx.set_data_root_infos_for_data_root(
                h(9),
                DataRootInfos(vec![
                    DataRootInfo {
                        start_offset: RelativeChunkOffset(-2),
                        data_size: 256,
                    },
                    DataRootInfo {
                        start_offset: RelativeChunkOffset(4),
                        data_size: 128,
                    },
                ]),
            )?;
            tx.add_pending_body_migration(
                PartitionChunkOffset(10),
                &PendingBodyMigration {
                    data_root: h(9),
                    data_size: 256,
                    start_offset: RelativeChunkOffset(-2),
                    block_height: 4,
                    attempts: 1,
                },
            )?;
            Ok(())
        })?;
        store.set_reset_in_progress()?;
        Ok(())
    }

    fn populate_mdbx(path: &Path) -> eyre::Result<()> {
        let store = MdbxSubmoduleStore::open(path, testing_args()?)?;
        populate(&store)
    }

    fn assert_synthetic(store: &impl SubmoduleStore) -> eyre::Result<()> {
        store.view(|tx| {
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(10))?,
                Some(b"tx-path-1".to_vec())
            );
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(12))?,
                Some(b"tx-path-1".to_vec())
            );
            assert_eq!(tx.get_tx_path_by_offset(PartitionChunkOffset(15))?, None);
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(20))?,
                Some(b"tx-path-2".to_vec())
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(10))?,
                Some(b"data-10".to_vec())
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(11))?,
                Some(b"data-11".to_vec())
            );
            assert_eq!(tx.get_data_path_by_offset(PartitionChunkOffset(12))?, None);
            let hashes = tx
                .get_path_hashes_by_offset(PartitionChunkOffset(10))?
                .expect("offset 10 hashes");
            assert_eq!(hashes.data_path_hash, Some(h(10)));
            assert_eq!(hashes.tx_path_hash, Some(h(1)));
            let tail = tx
                .get_path_hashes_by_offset(PartitionChunkOffset(12))?
                .expect("offset 12 hashes");
            assert_eq!(tail.data_path_hash, None);
            assert_eq!(tail.tx_path_hash, Some(h(1)));
            assert_eq!(tx.get_full_tx_path(h(2))?, Some(b"tx-path-2".to_vec()));
            assert_eq!(
                tx.get_tx_leaf_binding(h(1))?,
                Some(TxLeafBinding {
                    data_root: h(9),
                    prefix_hash: h(8),
                })
            );
            let infos = tx
                .get_data_root_infos_for_data_root(h(9))?
                .expect("data root");
            assert_eq!(
                infos.0,
                vec![
                    DataRootInfo {
                        start_offset: RelativeChunkOffset(-2),
                        data_size: 256,
                    },
                    DataRootInfo {
                        start_offset: RelativeChunkOffset(4),
                        data_size: 128,
                    },
                ]
            );
            let job = tx
                .get_pending_body_migration(PartitionChunkOffset(10))?
                .expect("pending job");
            assert_eq!(job.data_root, h(9));
            assert_eq!(job.start_offset, RelativeChunkOffset(-2));
            assert_eq!(job.block_height, 4);
            Ok(())
        })?;
        assert!(store.reset_in_progress()?);
        Ok(())
    }

    fn assert_copy_counts(stats: &super::ConvertStats) {
        assert_eq!(stats.tx_paths, 2);
        assert_eq!(stats.tx_leaf_bindings, 2);
        assert_eq!(stats.intervals, 2);
        assert_eq!(stats.path_hashes, 2);
        assert_eq!(stats.data_paths, 2);
        assert_eq!(stats.data_roots, 1);
        assert_eq!(stats.pending, 1);
        assert!(stats.reset_marker);
    }

    fn vm_kib(prefix: &str) -> u64 {
        let Ok(text) = fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        text.lines()
            .find_map(|line| {
                let rest = line.strip_prefix(prefix)?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .unwrap_or(0)
    }

    #[test]
    fn convert_round_trip_mdbx_rocks_mdbx() -> eyre::Result<()> {
        let rss_before = vm_kib("VmRSS:");
        let root = temp("convert-round");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let back = root.path().join("db-back");
        populate_mdbx(&mdbx)?;

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options()?)?;
        assert!(!stats.migrated_v1);
        assert_copy_counts(&stats);
        assert_eq!(fs::read_to_string(rocks.join("SCHEMA"))?, SUBMODULE_SCHEMA);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options()?)?;
        assert!(!report.has_mismatch(), "{report}");
        assert!(report.offsets_checked >= 2, "{report}");
        assert!(report.intervals_checked >= 2, "{report}");
        assert_eq!(report.tx_paths_checked, 2, "{report}");
        assert_eq!(report.data_roots_checked, 1, "{report}");
        assert_eq!(report.pending_checked, 1, "{report}");
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            assert_synthetic(&opened)?;
        }
        assert_synthetic(&MdbxSubmoduleStore::open(&mdbx, testing_args()?)?)?;

        let back_stats = convert_rocks_to_mdbx(&rocks, &back, &options()?)?;
        assert_copy_counts(&back_stats);
        let report = verify_mdbx_and_rocks(&back, &rocks, &options()?)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_synthetic(&MdbxSubmoduleStore::open(&back, testing_args()?)?)?;

        let mut replace = options()?;
        replace.replace_incomplete = true;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &replace).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");

        let rss_after = vm_kib("VmRSS:");
        let hwm = vm_kib("VmHWM:");
        let growth = rss_after.saturating_sub(rss_before);
        eprintln!("convert_round_trip VmRSS_KiB={rss_after} VmHWM_KiB={hwm} growth_KiB={growth}");
        assert!(hwm > 0, "VmHWM was not readable");
        assert!(growth < 256 * 1024, "round trip grew RSS by {growth} KiB");
        Ok(())
    }

    #[test]
    fn convert_interrupt_leaves_no_marker_and_can_be_redone() -> eyre::Result<()> {
        let root = temp("convert-interrupt");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        populate_mdbx(&mdbx)?;
        let mut broken = options()?;
        broken.batch_rows = 1;
        broken.fail_after_batches = Some(1);
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &broken).unwrap_err();
        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert!(!rocks.join("SCHEMA").exists());
        assert!(rocks.join("CURRENT").is_file());
        let open_err = RocksSubmoduleStore::open(&rocks).unwrap_err();
        let message = format!("{open_err:#}");
        assert!(message.contains("submodule-index-convert"), "{message}");
        assert!(message.contains("SCHEMA"), "{message}");
        assert_synthetic(&MdbxSubmoduleStore::open(&mdbx, testing_args()?)?)?;

        broken.fail_after_batches = None;
        broken.replace_incomplete = true;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &broken)?;
        assert_copy_counts(&stats);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options()?)?;
        assert!(!report.has_mismatch(), "{report}");

        let mdbx_back = root.path().join("db-back");
        broken.fail_after_batches = Some(1);
        broken.replace_incomplete = false;
        let err = convert_rocks_to_mdbx(&rocks, &mdbx_back, &broken).unwrap_err();
        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert_eq!(fs::read_to_string(rocks.join("SCHEMA"))?, SUBMODULE_SCHEMA);
        let open_err = MdbxSubmoduleStore::open(&mdbx_back, testing_args()?).unwrap_err();
        let message = format!("{open_err:#}");
        assert!(message.contains("incomplete"), "{message}");
        assert!(message.contains("submodule-index-convert"), "{message}");

        broken.fail_after_batches = None;
        broken.replace_incomplete = true;
        let stats = convert_rocks_to_mdbx(&rocks, &mdbx_back, &broken)?;
        assert_copy_counts(&stats);
        let report = verify_mdbx_and_rocks(&mdbx_back, &rocks, &options()?)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_synthetic(&MdbxSubmoduleStore::open(&mdbx_back, testing_args()?)?)?;
        Ok(())
    }

    #[test]
    fn convert_refuses_same_path_and_non_empty_target() -> eyre::Result<()> {
        let root = temp("convert-refuse-target");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        populate_mdbx(&mdbx)?;
        let err = convert_mdbx_to_rocks(&mdbx, &mdbx, &options()?).unwrap_err();
        assert!(format!("{err:#}").contains("same path"), "{err:#}");
        fs::create_dir_all(&rocks)?;
        fs::write(rocks.join("note"), b"keep")?;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options()?).unwrap_err();
        assert!(
            format!("{err:#}").contains("--replace-incomplete"),
            "{err:#}"
        );
        assert_eq!(fs::read(rocks.join("note"))?, b"keep");
        assert_synthetic(&MdbxSubmoduleStore::open(&mdbx, testing_args()?)?)?;
        Ok(())
    }

    #[test]
    fn verify_reports_a_row_only_one_engine_has() -> eyre::Result<()> {
        let root = temp("convert-mismatch");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        populate_mdbx(&mdbx)?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options()?)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| tx.add_full_tx_path(h(3), b"extra".to_vec()))?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options()?)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.tx_paths_mismatches >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn convert_migrates_v1_before_copy() -> eyre::Result<()> {
        use crate::open_or_create_db;
        use crate::submodule::tables::v1_fixture::master;
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let root = temp("convert-v1");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let db = open_or_create_db(&mdbx, master::MasterV1Tables::ALL, testing_args()?)?;
        {
            let tx = db.tx_mut()?;
            tx.put::<master::ChunkPathHashesByOffset>(
                PartitionChunkOffset(1),
                ChunkPathHashes {
                    data_path_hash: Some(h(4)),
                    tx_path_hash: Some(h(5)),
                },
            )?;
            tx.put::<master::ChunkDataPathByPathHash>(h(4), b"v1-data".to_vec())?;
            tx.put::<master::TxPathByTxPathHash>(h(5), b"v1-tx".to_vec())?;
            tx.commit()?;
        }
        drop(db);

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options()?)?;
        assert!(stats.migrated_v1);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options()?)?;
        assert!(!report.has_mismatch(), "{report}");
        let opened = RocksSubmoduleStore::open(&rocks)?;
        opened.view(|tx| {
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(1))?,
                Some(b"v1-data".to_vec())
            );
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(1))?,
                Some(b"v1-tx".to_vec())
            );
            let hashes = tx
                .get_path_hashes_by_offset(PartitionChunkOffset(1))?
                .expect("v1 hashes");
            assert_eq!(hashes.data_path_hash, Some(h(4)));
            assert_eq!(hashes.tx_path_hash, Some(h(5)));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn convert_refuses_when_an_opener_holds_the_lock() -> eyre::Result<()> {
        if let Ok(kind) = std::env::var("IRYS_CONVERT_LOCK_CHILD") {
            return hold_index_lock(&kind);
        }
        let root = temp("convert-lock");
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        populate_mdbx(&mdbx)?;

        let ready = root.path().join("mdbx.ready");
        let mut child = spawn_lock_child("mdbx", &mdbx, &ready)?;
        wait_ready(&ready, &mut child)?;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options()?).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        drop(child);
        assert_synthetic(&MdbxSubmoduleStore::open(&mdbx, testing_args()?)?)?;

        convert_mdbx_to_rocks(&mdbx, &rocks, &options()?)?;
        let ready = root.path().join("rocks.ready");
        let mut child = spawn_lock_child("rocks", &rocks, &ready)?;
        wait_ready(&ready, &mut child)?;
        let back = root.path().join("db-back");
        let err = convert_rocks_to_mdbx(&rocks, &back, &options()?).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        drop(child);
        assert_synthetic(&RocksSubmoduleStore::open(&rocks)?)?;
        Ok(())
    }

    struct KillChild(Child);

    impl Drop for KillChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn_lock_child(kind: &str, index: &Path, ready: &Path) -> eyre::Result<KillChild> {
        let child = Command::new(std::env::current_exe()?)
            .arg("convert_refuses_when_an_opener_holds_the_lock")
            .env("IRYS_CONVERT_LOCK_CHILD", kind)
            .env("IRYS_CONVERT_LOCK_PATH", index)
            .env("IRYS_CONVERT_LOCK_READY", ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(KillChild(child))
    }

    fn wait_ready(ready: &Path, child: &mut KillChild) -> eyre::Result<()> {
        let start = Instant::now();
        loop {
            if ready.is_file() {
                return Ok(());
            }
            if let Some(status) = child.0.try_wait()? {
                eyre::bail!("lock child exited before ready: {status}");
            }
            if start.elapsed() > Duration::from_secs(30) {
                eyre::bail!("lock child did not become ready");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn hold_index_lock(kind: &str) -> eyre::Result<()> {
        let path = PathBuf::from(std::env::var("IRYS_CONVERT_LOCK_PATH")?);
        let ready = PathBuf::from(std::env::var("IRYS_CONVERT_LOCK_READY")?);
        match kind {
            "mdbx" => {
                let args = testing_args()?.with_exclusive(Some(true));
                let store = MdbxSubmoduleStore::open(&path, args)?;
                fs::write(&ready, b"ready")?;
                park_until_killed();
                drop(store);
            }
            "rocks" => {
                let store = RocksSubmoduleStore::open(&path)?;
                fs::write(&ready, b"ready")?;
                park_until_killed();
                drop(store);
            }
            other => eyre::bail!("unknown lock child {other}"),
        }
        Ok(())
    }

    fn park_until_killed() {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(120) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}
