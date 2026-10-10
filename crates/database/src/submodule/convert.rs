//! Offline copy between submodule indexes.
//!
//! `mdbx-to-rocks` and `rocks-to-v3` write a RocksDB schema v3 index.
//! `rocks-to-mdbx` rebuilds an MDBX schema v2 index from schema v3. Each
//! stored tx path must equal the path derived from the consensus database.
//! A mismatch aborts and names the partition offset.
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
//! Verification reads both engines through the store read path. The tx path
//! check compares the stored v2 bytes with a fresh derive.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eyre::WrapErr as _;
use irys_types::{
    ChunkDataPath, DataLedger, H256, PartitionChunkOffset, RelativeChunkOffset, TxPath, TxPathHash,
    hash_sha256,
};
use reth_db::mdbx::DatabaseArguments;
use reth_db::table::Table as _;
use reth_db::{DatabaseEnv, DatabaseEnvKind};

use crate::db::IrysDatabaseExt as _;
use crate::tx_header_by_txid;
use crate::tx_path_derive::{derive_tx_path_at_offset, tx_path_hash};

use super::rocks::RocksSubmoduleStore;
use super::store::{MdbxSubmoduleStore, SubmoduleStore, SubmoduleWrite};
use super::tables::{
    ChunkDataPathByOffset, DataRootInfo, PendingBodyMigration, PendingBodyMigrationsByOffset,
    SUBMODULE_CONVERT_INCOMPLETE, SUBMODULE_SCHEMA, SUBMODULE_SCHEMA_V3, TxLeafBinding,
    TxPathInterval, TxPathIntervalByStart,
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
    /// Arguments for the read-only consensus database open.
    pub consensus: DatabaseArguments,
    /// Node consensus database. Required. The tool derives each tx path from it.
    pub consensus_db: Option<PathBuf>,
    /// Ledger id (`DataLedger::get_id`). Publish is 0.
    pub ledger_id: u32,
    /// Partition slot from `packing_params.toml`.
    pub slot_index: u64,
    /// Consensus `num_chunks_in_partition`.
    pub chunks_per_partition: u64,
    /// Consensus chunk size in bytes.
    pub chunk_size: u64,
    /// `Some(n)` returns an error after `n` committed row batches and does
    /// not write the completion marker. `Some(0)` returns before the first
    /// row batch. The target is left incomplete.
    pub fail_after_batches: Option<u32>,
}

impl ConvertOptions {
    /// Production batch size. `mdbx` should be the submodule arguments.
    /// The converter sets the exclusive lock itself. `consensus` starts as a
    /// clone of `mdbx`; the binary replaces it with the consensus arguments.
    pub fn new(mdbx: DatabaseArguments) -> Self {
        Self {
            batch_rows: 2048,
            batch_value_bytes: 8 * 1024 * 1024,
            replace_incomplete: false,
            consensus: mdbx.clone(),
            mdbx,
            consensus_db: None,
            ledger_id: 0,
            slot_index: 0,
            chunks_per_partition: 0,
            chunk_size: 0,
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
    fn read_intervals(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, TxPathInterval)>>;

    fn read_data_paths(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkDataPath)>>;

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
}

#[derive(Clone, Copy)]
enum OffsetTable {
    DataPaths,
    Pending,
}

impl RowSource for MdbxSubmoduleStore {
    fn read_intervals(
        &self,
        after: Option<PartitionChunkOffset>,
        limits: Limits,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, TxPathInterval)>> {
        self.read_rows::<TxPathIntervalByStart, _>(after, limits.rows, limits.value_bytes, |_| 40)
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
            OffsetTable::DataPaths => self.read_keys::<ChunkDataPathByOffset>(after, limit),
            OffsetTable::Pending => self.read_keys::<PendingBodyMigrationsByOffset>(after, limit),
        }
    }
}

impl RowSource for RocksSubmoduleStore {
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
            OffsetTable::DataPaths => ChunkDataPathByOffset::NAME,
            OffsetTable::Pending => PendingBodyMigrationsByOffset::NAME,
        };
        self.read_cf_keys(name, after, limit)
    }
}

/// Copies a v1 or v2 MDBX submodule index into a new RocksDB schema v3 directory.
///
/// A v1 source is migrated in place first. Each stored tx path must match the
/// path derived from the consensus database. A mismatch aborts and names the
/// partition offset. The RocksDB `SCHEMA` file is written only after the rows
/// are durable.
pub fn convert_mdbx_to_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    precheck(mdbx_dir, rocks_dir)?;
    let map = require_map(options)?;
    if !mdbx_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "source {} has no mdbx.dat. Pass the submodule db/ directory.",
            mdbx_dir.display()
        );
    }
    prepare_rocks_target(rocks_dir, options.replace_incomplete)?;
    let args = exclusive_args(options);
    let (src, migration) = map_open(MdbxSubmoduleStore::open_with_stats(mdbx_dir, args))?;
    let dst = map_open(RocksSubmoduleStore::open_converting_v3(rocks_dir))?;
    let db = open_consensus(options)?;
    let mut stats = copy_v2_into_v3(&src, &dst, options, &db, &map)?;
    stats.migrated_v1 = migration.migrated;
    if src.reset_in_progress()? {
        dst.set_reset_in_progress()?;
        stats.reset_marker = true;
    }
    dst.stamp_schema_marker()?;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(stats)
}

/// Copies a completed RocksDB schema v2 index into a new schema v3 directory.
///
/// The source is not modified. The check is the same as [`convert_mdbx_to_rocks`]:
/// the derived tx path must equal the stored bytes, or the run aborts with the
/// partition offset.
pub fn convert_rocks_v2_to_v3(
    v2_dir: &Path,
    v3_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    precheck(v2_dir, v3_dir)?;
    let map = require_map(options)?;
    match rocks_schema_text(v2_dir)? {
        Some(text) if text == SUBMODULE_SCHEMA_V3 => {
            eyre::bail!(
                "source {} is already schema v3. Refusing to convert it again.",
                v2_dir.display()
            );
        }
        Some(text) if text == SUBMODULE_SCHEMA => {}
        _ => eyre::bail!(
            "source {} has no schema v2 SCHEMA marker. Refusing to read an incomplete RocksDB index.",
            v2_dir.display()
        ),
    }
    prepare_rocks_target(v3_dir, options.replace_incomplete)?;
    let src = map_open(RocksSubmoduleStore::open(v2_dir))?;
    if !src.stores_tx_paths() {
        eyre::bail!("source {} is not a schema v2 index.", v2_dir.display());
    }
    let dst = map_open(RocksSubmoduleStore::open_converting_v3(v3_dir))?;
    let db = open_consensus(options)?;
    let mut stats = copy_v2_into_v3(&src, &dst, options, &db, &map)?;
    if src.reset_in_progress()? {
        dst.set_reset_in_progress()?;
        stats.reset_marker = true;
    }
    dst.stamp_schema_marker()?;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(stats)
}

/// Rebuilds an MDBX schema v2 index from a RocksDB schema v3 index.
///
/// Tx paths and leaf bindings are derived from the consensus database. A
/// stored height, tx index, or data root that disagrees with that derive
/// aborts the run and names the partition offset. The MDBX schema row stays
/// [`SUBMODULE_CONVERT_INCOMPLETE`] until every row is durable, then becomes
/// [`SUBMODULE_SCHEMA`].
pub fn convert_rocks_to_mdbx(
    rocks_dir: &Path,
    mdbx_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    precheck(rocks_dir, mdbx_dir)?;
    let map = require_map(options)?;
    require_rocks_v3(rocks_dir)?;
    let args = exclusive_args(options);
    prepare_mdbx_target(mdbx_dir, args.clone(), options.replace_incomplete)?;
    let src = map_open(RocksSubmoduleStore::open(rocks_dir))?;
    let dst = map_open(MdbxSubmoduleStore::open_for_convert(mdbx_dir, args))?;
    let db = open_consensus(options)?;
    let mut stats = copy_v3_into_v2(&src, &dst, options, &db, &map)?;
    if src.reset_in_progress()? {
        dst.set_reset_in_progress()?;
        stats.reset_marker = true;
    }
    dst.put_schema_bytes(SUBMODULE_SCHEMA.as_bytes())?;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(stats)
}

/// Compares a schema v2 MDBX index with a schema v3 RocksDB index.
///
/// Data paths, pending jobs, and placement fields are read from both engines.
/// The tx path is the stored v2 bytes on one side and the path derived from
/// the consensus database on the other. Exit the process when
/// [`EngineEquivalenceReport::has_mismatch`] is set.
///
/// Does not migrate and does not write. A v1 MDBX directory is refused.
pub fn verify_mdbx_and_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<EngineEquivalenceReport> {
    precheck(mdbx_dir, rocks_dir)?;
    let map = require_map(options)?;
    let args = exclusive_args(options);
    require_completed_mdbx(mdbx_dir, args.clone())?;
    require_rocks_v3(rocks_dir)?;
    let mdbx = map_open(MdbxSubmoduleStore::open_with_stats(mdbx_dir, args))?.0;
    let rocks = map_open(RocksSubmoduleStore::open(rocks_dir))?;
    let schema = mdbx.schema_bytes()?;
    let db = open_consensus(options)?;
    verify_v2_and_v3(&mdbx, &rocks, schema, &db, &map)
}

/// Compares a schema v2 RocksDB index with a schema v3 RocksDB index.
///
/// The tx-path check is the same as [`verify_mdbx_and_rocks`].
pub fn verify_rocks_v2_and_v3(
    v2_dir: &Path,
    v3_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<EngineEquivalenceReport> {
    precheck(v2_dir, v3_dir)?;
    let map = require_map(options)?;
    require_rocks_schema(v2_dir, SUBMODULE_SCHEMA, "schema v2")?;
    require_rocks_v3(v3_dir)?;
    let v2 = map_open(RocksSubmoduleStore::open(v2_dir))?;
    let v3 = map_open(RocksSubmoduleStore::open(v3_dir))?;
    if !v2.stores_tx_paths() {
        eyre::bail!("source {} is not a schema v2 index", v2_dir.display());
    }
    let schema = v2.schema_bytes()?;
    let db = open_consensus(options)?;
    verify_v2_and_v3(&v2, &v3, schema, &db, &map)
}

struct PlacementMap {
    ledger: DataLedger,
    slot_index: u64,
    chunks_per_partition: u64,
    chunk_size: u64,
}

impl PlacementMap {
    fn ledger_offset(&self, partition_offset: PartitionChunkOffset) -> eyre::Result<u64> {
        let base = self
            .slot_index
            .checked_mul(self.chunks_per_partition)
            .ok_or_else(|| {
                eyre::eyre!(
                    "slot {} times chunks per partition overflows",
                    self.slot_index
                )
            })?;
        base.checked_add(u64::from(partition_offset.0))
            .ok_or_else(|| {
                eyre::eyre!(
                    "ledger offset overflows at partition offset {}",
                    partition_offset.0
                )
            })
    }
}

struct PreparedPlacement {
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    path_hash: TxPathHash,
    data_root: H256,
    prefix_hash: H256,
    info: Option<DataRootInfo>,
    ledger_id: u32,
    block_height: u64,
    tx_index: u32,
    had_binding: bool,
}

fn require_map(options: &ConvertOptions) -> eyre::Result<PlacementMap> {
    if options.consensus_db.is_none() {
        eyre::bail!(
            "pass --consensus-db, the node consensus database. The converter derives each tx path from it and refuses a byte mismatch."
        );
    }
    if options.chunk_size == 0 || options.chunks_per_partition == 0 {
        eyre::bail!("--chunk-size and --chunks-per-partition must be non-zero");
    }
    let ledger = DataLedger::try_from(options.ledger_id).wrap_err_with(|| {
        format!(
            "ledger id {} is not a data ledger. Publish is 0, Submit is 1.",
            options.ledger_id
        )
    })?;
    Ok(PlacementMap {
        ledger,
        slot_index: options.slot_index,
        chunks_per_partition: options.chunks_per_partition,
        chunk_size: options.chunk_size,
    })
}

fn open_consensus(options: &ConvertOptions) -> eyre::Result<DatabaseEnv> {
    let path = options
        .consensus_db
        .as_ref()
        .ok_or_else(|| eyre::eyre!("pass --consensus-db"))?;
    if !path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "consensus database {} has no mdbx.dat. Pass the node db directory.",
            path.display()
        );
    }
    let args = options.consensus.clone().with_exclusive(Some(false));
    DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .map_err(|err| eyre::eyre!("read-only open {}: {err}", path.display()))
}

fn copy_v2_into_v3<S>(
    src: &S,
    dst: &RocksSubmoduleStore,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats>
where
    S: RowSource + SubmoduleStore,
{
    let mut progress = CopyProgress {
        batches: 0,
        fail_after_batches: options.fail_after_batches,
    };
    if progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limits = Limits::from_options(options);
    let mut tx_paths = 0_u64;
    let mut bindings = 0_u64;
    let mut infos = 0_u64;
    let intervals = copy_rows(
        |after| src.read_intervals(after, limits),
        |batch| {
            let mut prepared = Vec::with_capacity(batch.len());
            for (start, interval) in &batch {
                let item = prepare_v2_placement(src, db, map, *start, interval)?;
                if item.had_binding {
                    bindings = bindings.saturating_add(1);
                }
                if item.info.is_some() {
                    infos = infos.saturating_add(1);
                }
                tx_paths = tx_paths.saturating_add(1);
                prepared.push(item);
            }
            dst.update(|tx| {
                for item in &prepared {
                    write_v3_placement(tx, item)?;
                }
                Ok(())
            })
        },
        &mut progress,
    )?;
    let data_paths = copy_rows(
        |after| src.read_data_paths(after, limits),
        |batch| {
            dst.update(|tx| {
                for (offset, path) in batch {
                    require_interval(tx, offset, "data path")?;
                    tx.add_full_data_path(offset, path)?;
                }
                Ok(())
            })
        },
        &mut progress,
    )?;
    let pending = copy_rows(
        |after| src.read_pending(after, limits),
        |batch| {
            dst.update(|tx| {
                for (offset, job) in &batch {
                    require_interval(tx, *offset, "pending body migration")?;
                    tx.add_pending_body_migration(*offset, job)?;
                }
                Ok(())
            })
        },
        &mut progress,
    )?;
    Ok(ConvertStats {
        tx_paths,
        tx_leaf_bindings: bindings,
        intervals,
        data_paths,
        data_roots: infos,
        pending,
        ..Default::default()
    })
}

fn copy_v3_into_v2(
    src: &RocksSubmoduleStore,
    dst: &MdbxSubmoduleStore,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats> {
    let mut progress = CopyProgress {
        batches: 0,
        fail_after_batches: options.fail_after_batches,
    };
    if progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limit = options.batch_rows.max(1);
    let mut after = None;
    let mut intervals = 0_u64;
    let mut infos = 0_u64;
    loop {
        let batch = src.read_v3_placements(after, limit)?;
        let Some(last) = batch.last().map(|row| row.start) else {
            break;
        };
        intervals = intervals.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        let mut rebuilt = Vec::with_capacity(batch.len());
        for placement in &batch {
            let item = rebuild_from_placement(db, map, placement)?;
            if item.info.is_some() {
                infos = infos.saturating_add(1);
            }
            rebuilt.push(item);
        }
        dst.update(|tx| {
            for item in &rebuilt {
                write_v2_placement(tx, item)?;
            }
            Ok(())
        })?;
        progress.note()?;
        after = Some(last);
    }
    let data_paths = copy_keyed(
        |after| src.read_data_path_keys(after, limit),
        |keys| {
            let rows = src.view(|tx| {
                keys.iter()
                    .map(|offset| {
                        let path = tx.get_data_path_by_offset(*offset)?;
                        Ok((*offset, path))
                    })
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            dst.update(|tx| {
                for (offset, path) in rows {
                    let Some(path) = path else {
                        eyre::bail!("data path key {} has no bytes", offset.0);
                    };
                    require_interval(tx, offset, "data path")?;
                    let hash = H256::from(hash_sha256(&path));
                    tx.add_full_data_path(offset, path)?;
                    tx.add_data_path_hash_to_offset_index(offset, Some(hash))?;
                }
                Ok(())
            })
        },
        &mut progress,
    )?;
    let pending = copy_keyed(
        |after| src.read_pending_keys(after, limit),
        |keys| {
            let rows = src.view(|tx| {
                keys.iter()
                    .map(|offset| {
                        let job = tx.get_pending_body_migration(*offset)?;
                        Ok((*offset, job))
                    })
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            dst.update(|tx| {
                for (offset, job) in rows {
                    let Some(job) = job else {
                        eyre::bail!("pending key {} has no job", offset.0);
                    };
                    require_interval(tx, offset, "pending body migration")?;
                    tx.add_pending_body_migration(offset, &job)?;
                }
                Ok(())
            })
        },
        &mut progress,
    )?;
    Ok(ConvertStats {
        tx_paths: intervals,
        tx_leaf_bindings: intervals,
        intervals,
        data_paths,
        data_roots: infos,
        pending,
        ..Default::default()
    })
}

fn copy_keyed<K>(
    mut read: impl FnMut(Option<K>) -> eyre::Result<Vec<K>>,
    mut write: impl FnMut(&[K]) -> eyre::Result<()>,
    progress: &mut CopyProgress,
) -> eyre::Result<u64>
where
    K: Copy,
{
    let mut after = None;
    let mut copied = 0_u64;
    loop {
        let batch = read(after)?;
        let Some(last) = batch.last().copied() else {
            break;
        };
        copied = copied.saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        write(&batch)?;
        progress.note()?;
        after = Some(last);
    }
    Ok(copied)
}

fn require_interval(
    tx: &mut dyn SubmoduleWrite,
    offset: PartitionChunkOffset,
    kind: &str,
) -> eyre::Result<()> {
    if tx.tx_interval_at(offset)?.is_none() {
        eyre::bail!(
            "{kind} at partition offset {} is outside a tx interval",
            offset.0
        );
    }
    Ok(())
}

fn prepare_v2_placement<S: SubmoduleStore>(
    src: &S,
    db: &DatabaseEnv,
    map: &PlacementMap,
    start: PartitionChunkOffset,
    interval: &TxPathInterval,
) -> eyre::Result<PreparedPlacement> {
    let stored = src
        .view(|tx| tx.get_full_tx_path(interval.tx_path_hash))?
        .ok_or_else(|| eyre::eyre!("partition offset {} has no stored tx path", start.0))?;
    let derived = derive_checked(db, map, start, interval.end, &stored)?;
    let binding = src.view(|tx| tx.get_tx_leaf_binding(interval.tx_path_hash))?;
    let header = db
        .view_eyre(|tx| tx_header_by_txid(tx, &derived.tx_id))?
        .ok_or_else(|| {
            eyre::eyre!(
                "tx header {} missing for partition offset {}",
                derived.tx_id,
                start.0
            )
        })?;
    if let Some(binding) = &binding
        && (binding.data_root != derived.data_root || binding.prefix_hash != header.prefix_hash)
    {
        eyre::bail!(
            "tx leaf binding at partition offset {} does not match the derived tx",
            start.0
        );
    }
    let info = covering_info_at(src, derived.data_root, start)?;
    let tx_index = u32::try_from(derived.tx_index)
        .map_err(|_| eyre::eyre!("tx index at partition offset {} exceeds u32", start.0))?;
    Ok(PreparedPlacement {
        start,
        end: interval.end,
        path_hash: interval.tx_path_hash,
        data_root: derived.data_root,
        prefix_hash: header.prefix_hash,
        info,
        ledger_id: map.ledger.get_id(),
        block_height: derived.height,
        tx_index,
        had_binding: binding.is_some(),
    })
}

fn derive_checked(
    db: &DatabaseEnv,
    map: &PlacementMap,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    stored: &[u8],
) -> eyre::Result<crate::tx_path_derive::DerivedTxPath> {
    let start_ledger = map.ledger_offset(start)?;
    let derived = derive_tx_path_at_offset(db, map.ledger, start_ledger, map.chunk_size)
        .wrap_err_with(|| format!("derive tx path at partition offset {}", start.0))?;
    if derived.proof.proof.as_slice() != stored {
        eyre::bail!(
            "tx path at partition offset {} (ledger {} offset {start_ledger}) does not match the derived path",
            start.0,
            map.ledger.get_id()
        );
    }
    if end != start {
        let end_ledger = map.ledger_offset(end)?;
        let at_end = derive_tx_path_at_offset(db, map.ledger, end_ledger, map.chunk_size)
            .wrap_err_with(|| format!("derive tx path at partition offset {}", end.0))?;
        if at_end.height != derived.height || at_end.tx_index != derived.tx_index {
            eyre::bail!(
                "partition offset {} through {} covers more than one tx",
                start.0,
                end.0
            );
        }
    }
    Ok(derived)
}

fn covering_info_at<S: SubmoduleStore>(
    src: &S,
    data_root: H256,
    probe: PartitionChunkOffset,
) -> eyre::Result<Option<DataRootInfo>> {
    let Some(infos) = src.view(|tx| tx.get_data_root_infos_for_data_root(data_root))? else {
        return Ok(None);
    };
    Ok(select_covering_info(&infos.0, probe))
}

fn select_covering_info(
    infos: &[DataRootInfo],
    probe: PartitionChunkOffset,
) -> Option<DataRootInfo> {
    if infos.is_empty() {
        return None;
    }
    let mut sorted = infos.to_vec();
    sorted.sort_unstable();
    let probe_rel = RelativeChunkOffset::from(probe);
    let index = sorted
        .partition_point(|info| info.start_offset <= probe_rel)
        .saturating_sub(1);
    let info = sorted.get(index)?;
    (info.start_offset <= probe_rel).then(|| info.clone())
}

fn write_v3_placement(tx: &mut dyn SubmoduleWrite, item: &PreparedPlacement) -> eyre::Result<()> {
    tx.note_placement_origin(item.ledger_id, item.block_height, item.tx_index)?;
    tx.add_tx_leaf_binding(
        item.path_hash,
        &TxLeafBinding {
            data_root: item.data_root,
            prefix_hash: item.prefix_hash,
        },
    )?;
    tx.add_tx_path_hash_to_offset_range(item.start, item.end, Some(item.path_hash))?;
    if let Some(info) = &item.info {
        tx.add_data_root_info(item.data_root, info)?;
    }
    Ok(())
}

struct RebuiltPlacement {
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    path_hash: TxPathHash,
    proof: TxPath,
    data_root: H256,
    prefix_hash: H256,
    info: Option<DataRootInfo>,
}

fn rebuild_from_placement(
    db: &DatabaseEnv,
    map: &PlacementMap,
    placement: &super::tables::Placement,
) -> eyre::Result<RebuiltPlacement> {
    let start_ledger = map.ledger_offset(placement.start)?;
    let derived = derive_tx_path_at_offset(db, map.ledger, start_ledger, map.chunk_size)
        .wrap_err_with(|| format!("derive tx path at partition offset {}", placement.start.0))?;
    if placement.data_root != derived.data_root {
        eyre::bail!(
            "data root at partition offset {} does not match the derived tx",
            placement.start.0
        );
    }
    if let Some(height) = placement.block_height
        && height != derived.height
    {
        eyre::bail!(
            "block height at partition offset {} does not match the derived tx",
            placement.start.0
        );
    }
    if let Some(tx_index) = placement.tx_index
        && tx_index as usize != derived.tx_index
    {
        eyre::bail!(
            "tx index at partition offset {} does not match the derived tx",
            placement.start.0
        );
    }
    if placement.ledger.is_some_and(|id| id != map.ledger.get_id()) {
        eyre::bail!(
            "ledger at partition offset {} does not match --ledger {}",
            placement.start.0,
            map.ledger.get_id()
        );
    }
    if placement.end != placement.start {
        let end_ledger = map.ledger_offset(placement.end)?;
        let at_end = derive_tx_path_at_offset(db, map.ledger, end_ledger, map.chunk_size)
            .wrap_err_with(|| format!("derive tx path at partition offset {}", placement.end.0))?;
        if at_end.height != derived.height || at_end.tx_index != derived.tx_index {
            eyre::bail!(
                "partition offset {} through {} covers more than one tx",
                placement.start.0,
                placement.end.0
            );
        }
    }
    let header = db
        .view_eyre(|tx| tx_header_by_txid(tx, &derived.tx_id))?
        .ok_or_else(|| {
            eyre::eyre!(
                "tx header {} missing for partition offset {}",
                derived.tx_id,
                placement.start.0
            )
        })?;
    let info = if placement.tx_start_offset.0 == 0 && placement.data_size == 0 {
        None
    } else {
        Some(DataRootInfo {
            start_offset: placement.tx_start_offset,
            data_size: placement.data_size,
        })
    };
    Ok(RebuiltPlacement {
        start: placement.start,
        end: placement.end,
        path_hash: tx_path_hash(&derived.proof.proof),
        proof: derived.proof.proof,
        data_root: derived.data_root,
        prefix_hash: header.prefix_hash,
        info,
    })
}

fn write_v2_placement(tx: &mut dyn SubmoduleWrite, item: &RebuiltPlacement) -> eyre::Result<()> {
    tx.add_full_tx_path(item.path_hash, item.proof.clone())?;
    tx.add_tx_leaf_binding(
        item.path_hash,
        &TxLeafBinding {
            data_root: item.data_root,
            prefix_hash: item.prefix_hash,
        },
    )?;
    tx.add_tx_path_hash_to_offset_range(item.start, item.end, Some(item.path_hash))?;
    if let Some(info) = &item.info {
        tx.add_data_root_info(item.data_root, info)?;
    }
    Ok(())
}

fn verify_v2_and_v3<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    v2_schema: Option<Vec<u8>>,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<EngineEquivalenceReport>
where
    S: RowSource + SubmoduleStore,
{
    let mut report = EngineEquivalenceReport::default();
    verify_data_paths(v2, v3, &mut report)?;
    verify_placements(v2, v3, db, map, &mut report)?;
    verify_pending_rows(v2, v3, &mut report)?;
    verify_markers(v2, v3, v2_schema, &mut report)?;
    Ok(report)
}

fn verify_data_paths<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()>
where
    S: RowSource + SubmoduleStore,
{
    merge_keys(
        &mut |after| v2.read_offset_keys(OffsetTable::DataPaths, after, VERIFY_KEYS),
        &mut |after| v3.read_data_path_keys(after, VERIFY_KEYS),
        |keys| {
            let left = v2.view(|tx| read_data_paths_at(tx, keys))?;
            let right = v3.view(|tx| read_data_paths_at(tx, keys))?;
            for (index, key) in keys.iter().enumerate() {
                report.offsets_checked = report.offsets_checked.saturating_add(1);
                if left[index] != right[index] {
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                    report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                    note(report, "data_path", format!("offset {}", key.0));
                }
            }
            Ok(())
        },
    )
}

fn read_data_paths_at(
    tx: &mut dyn super::store::SubmoduleRead,
    keys: &[PartitionChunkOffset],
) -> eyre::Result<Vec<Option<ChunkDataPath>>> {
    keys.iter()
        .map(|key| tx.get_data_path_by_offset(*key))
        .collect()
}

fn verify_placements<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    map: &PlacementMap,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()>
where
    S: RowSource + SubmoduleStore,
{
    let limits = Limits {
        rows: VERIFY_KEYS,
        value_bytes: usize::MAX,
    };
    merge_keys(
        &mut |after| {
            Ok(v2
                .read_intervals(after, limits)?
                .into_iter()
                .map(|(start, _)| start)
                .collect())
        },
        &mut |after| {
            Ok(v3
                .read_v3_placements(after, VERIFY_KEYS)?
                .into_iter()
                .map(|row| row.start)
                .collect())
        },
        |keys| {
            for key in keys {
                report.intervals_checked = report.intervals_checked.saturating_add(1);
                report.tx_paths_checked = report.tx_paths_checked.saturating_add(1);
                compare_one_placement(v2, v3, db, map, *key, report)?;
            }
            Ok(())
        },
    )
}

fn compare_one_placement<S: SubmoduleStore>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    map: &PlacementMap,
    key: PartitionChunkOffset,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let interval = v2.view(|tx| tx.tx_interval_at(key))?;
    let place = v3.view(|tx| tx.placement_at(key))?;
    let (Some(interval), Some(place)) = (interval, place) else {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    };
    if interval.start != key || place.start != key || interval.end != place.end {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    }
    let Some(stored) = v2.view(|tx| tx.get_full_tx_path(interval.tx_path_hash))? else {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        note(report, "tx_path", format!("offset {}", key.0));
        return Ok(());
    };
    let derived =
        match derive_tx_path_at_offset(db, map.ledger, map.ledger_offset(key)?, map.chunk_size) {
            Ok(derived) => derived,
            Err(err) => {
                report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
                note(report, "tx_path", format!("offset {} ({err})", key.0));
                return Ok(());
            }
        };
    if derived.proof.proof != stored {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        note(report, "tx_path", format!("offset {}", key.0));
        return Ok(());
    }
    let tx_index = u32::try_from(derived.tx_index).ok();
    let same = place.data_root == derived.data_root
        && place.end == interval.end
        && place.block_height == Some(derived.height)
        && place.tx_index == tx_index
        && place.ledger == Some(map.ledger.get_id());
    if !same {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
    }
    report.data_roots_checked = report.data_roots_checked.saturating_add(1);
    let binding = v2.view(|tx| tx.get_tx_leaf_binding(interval.tx_path_hash))?;
    if let Some(binding) = binding {
        report.tx_leaf_bindings_checked = report.tx_leaf_bindings_checked.saturating_add(1);
        if binding.data_root != derived.data_root {
            report.tx_leaf_bindings_mismatches =
                report.tx_leaf_bindings_mismatches.saturating_add(1);
            note(report, "tx_leaf_binding", format!("offset {}", key.0));
        }
        match v2.view(|tx| tx.placement_at(key))? {
            Some(v2_place)
                if v2_place.tx_start_offset != place.tx_start_offset
                    || v2_place.data_size != place.data_size =>
            {
                report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
                note(report, "data_root", format!("offset {}", key.0));
            }
            None if place.tx_start_offset.0 != 0 || place.data_size != 0 => {
                report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
                note(report, "data_root", format!("offset {}", key.0));
            }
            _ => {}
        }
    } else if place.tx_start_offset.0 != 0 || place.data_size != 0 {
        report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
        note(report, "data_root", format!("offset {}", key.0));
    }
    Ok(())
}

fn verify_pending_rows<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()>
where
    S: RowSource + SubmoduleStore,
{
    merge_keys(
        &mut |after| v2.read_offset_keys(OffsetTable::Pending, after, VERIFY_KEYS),
        &mut |after| v3.read_pending_keys(after, VERIFY_KEYS),
        |keys| {
            let left = v2.view(|read| read_pending_at(read, keys))?;
            let right = v3.view(|read| read_pending_at(read, keys))?;
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

fn read_pending_at(
    tx: &mut dyn super::store::SubmoduleRead,
    keys: &[PartitionChunkOffset],
) -> eyre::Result<Vec<Option<PendingBodyMigration>>> {
    keys.iter()
        .map(|key| tx.get_pending_body_migration(*key))
        .collect()
}

fn verify_markers<S: SubmoduleStore>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    v2_schema: Option<Vec<u8>>,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    if v2.reset_in_progress()? != v3.reset_in_progress()? {
        report.reset_marker_mismatch = 1;
        note(report, "reset_marker", "metadata".to_owned());
    }
    let v3_schema = v3.schema_bytes()?;
    let v2_ok = v2_schema.as_deref() == Some(SUBMODULE_SCHEMA.as_bytes());
    let v3_ok = v3_schema.as_deref() == Some(SUBMODULE_SCHEMA_V3.as_bytes());
    if !v2_ok || !v3_ok {
        report.schema_mismatch = 1;
        note(report, "schema", "metadata".to_owned());
    }
    Ok(())
}

fn rocks_schema_text(path: &Path) -> eyre::Result<Option<String>> {
    let marker = path.join("SCHEMA");
    if !marker.is_file() {
        return Ok(None);
    }
    fs::read_to_string(&marker)
        .wrap_err("read SCHEMA")
        .map(Some)
}

fn require_rocks_schema(path: &Path, expected: &str, label: &str) -> eyre::Result<()> {
    match rocks_schema_text(path)? {
        Some(text) if text == expected => Ok(()),
        _ => eyre::bail!(
            "rocks index {} has no {label} SCHEMA marker",
            path.display()
        ),
    }
}

fn require_rocks_v3(path: &Path) -> eyre::Result<()> {
    match rocks_schema_text(path)? {
        Some(text) if text == SUBMODULE_SCHEMA_V3 => Ok(()),
        Some(text) if text == SUBMODULE_SCHEMA => eyre::bail!(
            "rocks index {} is schema v2. Run `submodule-index-convert rocks-to-v3` to write schema v3. `rocks-to-mdbx` rebuilds MDBX from schema v3.",
            path.display()
        ),
        _ => eyre::bail!(
            "rocks index {} has no schema v3 SCHEMA marker",
            path.display()
        ),
    }
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
    match rocks_schema_text(path)? {
        Some(text) => Ok(text == SUBMODULE_SCHEMA || text == SUBMODULE_SCHEMA_V3),
        None => Ok(false),
    }
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
    use irys_types::{
        DataLedger, DataTransactionHeader, DataTransactionLedger, H256, H256List, IrysBlockHeader,
        PartitionChunkOffset, RelativeChunkOffset, hash_sha256,
    };
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::{DatabaseEnv, DatabaseEnvKind};

    use super::super::store::{MdbxSubmoduleStore, SubmoduleStore};
    use super::super::tables::{
        ChunkPathHashes, DataRootInfo, PendingBodyMigration, SUBMODULE_SCHEMA_V3, TxLeafBinding,
    };
    use super::{
        ConvertOptions, convert_mdbx_to_rocks, convert_rocks_to_mdbx, convert_rocks_v2_to_v3,
        verify_mdbx_and_rocks, verify_rocks_v2_and_v3,
    };
    use crate::db::IrysDatabaseExt as _;
    use crate::submodule::rocks::{RocksSubmoduleStore, RocksTuning};
    use crate::tables::IrysTables;
    use crate::tx_path_derive::{derive_tx_path_at_offset, tx_path_hash};
    use crate::{
        IrysDatabaseArgs as _, insert_block_header, insert_block_index_items_for_block,
        insert_tx_header, open_or_create_db,
    };

    fn testing_args() -> eyre::Result<DatabaseArguments> {
        DatabaseArguments::irys_testing()
    }

    fn options_for(consensus: &Path) -> eyre::Result<ConvertOptions> {
        let mut options = ConvertOptions::new(testing_args()?);
        options.consensus = testing_args()?;
        options.consensus_db = Some(consensus.to_path_buf());
        options.ledger_id = DataLedger::Publish.get_id();
        options.slot_index = 1;
        options.chunks_per_partition = 8;
        options.chunk_size = 32;
        Ok(options)
    }

    fn temp(prefix: &str) -> tempfile::TempDir {
        TempDirBuilder::new().prefix(prefix).build()
    }

    fn hid(n: u64) -> H256 {
        let mut bytes = [0_u8; 32];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        H256::from(bytes)
    }

    fn tx_at(n: u64, size: u64) -> DataTransactionHeader {
        let mut header = DataTransactionHeader::default();
        header.id = hid(n);
        header.data_root = hid(n.saturating_add(100));
        header.prefix_hash = hid(n.saturating_add(200));
        header.data_size = size;
        header
    }

    fn set_ledger(
        block: &mut IrysBlockHeader,
        ledger: DataLedger,
        txs: &[DataTransactionHeader],
        total: u64,
    ) {
        let (root, _) = DataTransactionLedger::merklize_tx_root(txs);
        let slot = block
            .data_ledgers
            .iter_mut()
            .find(|item| item.ledger_id == ledger.get_id())
            .expect("mock header has the ledger");
        slot.tx_ids = H256List(txs.iter().map(|header| header.id).collect());
        slot.tx_root = root;
        slot.total_chunks = total;
    }

    fn block_with(
        height: u64,
        publish: &[DataTransactionHeader],
        publish_total: u64,
    ) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = height;
        block.block_hash = hid(height.saturating_add(1));
        set_ledger(&mut block, DataLedger::Publish, publish, publish_total);
        set_ledger(&mut block, DataLedger::Submit, &[], 0);
        block
    }

    fn write_chain(path: &Path) -> eyre::Result<()> {
        fs::create_dir_all(path)?;
        let tx_a = tx_at(1, 352);
        let tx_b = tx_at(2, 32);
        let tx_c = tx_at(3, 32);
        let blocks = [
            block_with(0, &[], 0),
            block_with(1, std::slice::from_ref(&tx_a), 11),
            block_with(2, &[tx_b.clone(), tx_c.clone()], 13),
        ];
        let db = open_or_create_db(path, IrysTables::ALL, testing_args()?)?;
        db.update_eyre(|tx| {
            for header in [&tx_a, &tx_b, &tx_c] {
                insert_tx_header(tx, header)?;
            }
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            Ok(())
        })?;
        drop(db);
        Ok(())
    }

    fn open_consensus(path: &Path) -> eyre::Result<DatabaseEnv> {
        DatabaseEnv::open(
            path,
            DatabaseEnvKind::RO,
            testing_args()?.with_exclusive(Some(false)),
        )
        .map_err(|err| eyre::eyre!("open consensus: {err}"))
    }

    struct DerivedPair {
        proof_a: Vec<u8>,
        proof_b: Vec<u8>,
        root_a: H256,
        root_b: H256,
        prefix_a: H256,
        prefix_b: H256,
    }

    fn derived_pair(consensus: &Path) -> eyre::Result<DerivedPair> {
        let db = open_consensus(consensus)?;
        let a = derive_tx_path_at_offset(&db, DataLedger::Publish, 8, 32)?;
        let b = derive_tx_path_at_offset(&db, DataLedger::Publish, 11, 32)?;
        let header_a = tx_at(1, 352);
        let header_b = tx_at(2, 32);
        Ok(DerivedPair {
            proof_a: a.proof.proof,
            proof_b: b.proof.proof,
            root_a: a.data_root,
            root_b: b.data_root,
            prefix_a: header_a.prefix_hash,
            prefix_b: header_b.prefix_hash,
        })
    }

    fn write_index(store: &impl SubmoduleStore, consensus: &Path) -> eyre::Result<()> {
        let pair = derived_pair(consensus)?;
        let hash_a = tx_path_hash(&pair.proof_a);
        let hash_b = tx_path_hash(&pair.proof_b);
        let data_0 = b"data-0".to_vec();
        let data_3 = b"data-3".to_vec();
        store.update(|tx| {
            tx.add_full_tx_path(hash_a, pair.proof_a.clone())?;
            tx.add_full_tx_path(hash_b, pair.proof_b.clone())?;
            tx.add_tx_leaf_binding(
                hash_a,
                &TxLeafBinding {
                    data_root: pair.root_a,
                    prefix_hash: pair.prefix_a,
                },
            )?;
            tx.add_tx_leaf_binding(
                hash_b,
                &TxLeafBinding {
                    data_root: pair.root_b,
                    prefix_hash: pair.prefix_b,
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(0),
                PartitionChunkOffset(2),
                Some(hash_a),
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(3),
                PartitionChunkOffset(3),
                Some(hash_b),
            )?;
            tx.add_full_data_path(PartitionChunkOffset(0), data_0.clone())?;
            tx.add_data_path_hash_to_offset_index(
                PartitionChunkOffset(0),
                Some(H256::from(hash_sha256(&data_0))),
            )?;
            tx.add_full_data_path(PartitionChunkOffset(3), data_3.clone())?;
            tx.add_data_path_hash_to_offset_index(
                PartitionChunkOffset(3),
                Some(H256::from(hash_sha256(&data_3))),
            )?;
            tx.add_data_root_info(
                pair.root_a,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(-8),
                    data_size: 352,
                },
            )?;
            tx.add_data_root_info(
                pair.root_b,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(3),
                    data_size: 32,
                },
            )?;
            tx.add_pending_body_migration(
                PartitionChunkOffset(0),
                &PendingBodyMigration {
                    data_root: pair.root_a,
                    data_size: 352,
                    start_offset: RelativeChunkOffset(-8),
                    block_height: 1,
                    attempts: 1,
                },
            )?;
            Ok(())
        })?;
        store.set_reset_in_progress()?;
        Ok(())
    }

    fn populate_mdbx(root: &Path) -> eyre::Result<PathBuf> {
        let consensus = root.join("consensus");
        write_chain(&consensus)?;
        let store = MdbxSubmoduleStore::open(root.join("db"), testing_args()?)?;
        write_index(&store, &consensus)?;
        Ok(consensus)
    }

    fn assert_v2(store: &impl SubmoduleStore, consensus: &Path) -> eyre::Result<()> {
        let pair = derived_pair(consensus)?;
        store.view(|tx| {
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(0))?,
                Some(pair.proof_a.clone())
            );
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(2))?,
                Some(pair.proof_a)
            );
            assert_eq!(
                tx.get_tx_path_by_offset(PartitionChunkOffset(3))?,
                Some(pair.proof_b)
            );
            assert_eq!(tx.get_tx_path_by_offset(PartitionChunkOffset(4))?, None);
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(0))?,
                Some(b"data-0".to_vec())
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(3))?,
                Some(b"data-3".to_vec())
            );
            let place = tx
                .placement_at(PartitionChunkOffset(0))?
                .expect("placement 0");
            assert_eq!(place.data_root, pair.root_a);
            assert_eq!(place.tx_start_offset, RelativeChunkOffset(-8));
            assert_eq!(place.data_size, 352);
            assert_eq!(place.end, PartitionChunkOffset(2));
            assert_eq!(place.ledger, None);
            let tail = tx
                .placement_at(PartitionChunkOffset(3))?
                .expect("placement 3");
            assert_eq!(tail.data_root, pair.root_b);
            assert_eq!(tail.tx_start_offset, RelativeChunkOffset(3));
            assert_eq!(tail.data_size, 32);
            let job = tx
                .get_pending_body_migration(PartitionChunkOffset(0))?
                .expect("pending");
            assert_eq!(job.data_root, pair.root_a);
            assert_eq!(job.start_offset, RelativeChunkOffset(-8));
            assert_eq!(job.block_height, 1);
            Ok(())
        })?;
        assert!(store.reset_in_progress()?);
        Ok(())
    }

    fn assert_v3(store: &impl SubmoduleStore, consensus: &Path) -> eyre::Result<()> {
        let pair = derived_pair(consensus)?;
        store.view(|tx| {
            assert_eq!(tx.get_tx_path_by_offset(PartitionChunkOffset(0))?, None);
            assert_eq!(tx.get_full_tx_path(tx_path_hash(&pair.proof_a))?, None);
            let place = tx
                .placement_at(PartitionChunkOffset(1))?
                .expect("placement covers 1");
            assert_eq!(place.start, PartitionChunkOffset(0));
            assert_eq!(place.end, PartitionChunkOffset(2));
            assert_eq!(place.data_root, pair.root_a);
            assert_eq!(place.tx_start_offset, RelativeChunkOffset(-8));
            assert_eq!(place.data_size, 352);
            assert_eq!(place.ledger, Some(DataLedger::Publish.get_id()));
            assert_eq!(place.block_height, Some(1));
            assert_eq!(place.tx_index, Some(0));
            let tail = tx
                .placement_at(PartitionChunkOffset(3))?
                .expect("placement 3");
            assert_eq!(tail.data_root, pair.root_b);
            assert_eq!(tail.block_height, Some(2));
            assert_eq!(tail.tx_index, Some(0));
            assert_eq!(tail.data_size, 32);
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(0))?,
                Some(b"data-0".to_vec())
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(3))?,
                Some(b"data-3".to_vec())
            );
            let job = tx
                .get_pending_body_migration(PartitionChunkOffset(0))?
                .expect("pending");
            assert_eq!(job.data_root, pair.root_a);
            Ok(())
        })?;
        assert!(store.reset_in_progress()?);
        Ok(())
    }

    fn assert_copy_counts(stats: &super::ConvertStats) {
        assert_eq!(stats.tx_paths, 2);
        assert_eq!(stats.tx_leaf_bindings, 2);
        assert_eq!(stats.intervals, 2);
        assert_eq!(stats.path_hashes, 0);
        assert_eq!(stats.data_paths, 2);
        assert_eq!(stats.data_roots, 2);
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

    fn assert_equivalent(report: &super::EngineEquivalenceReport) {
        assert!(!report.has_mismatch(), "{report}");
        assert!(report.offsets_checked >= 2, "{report}");
        assert_eq!(report.intervals_checked, 2, "{report}");
        assert_eq!(report.tx_paths_checked, 2, "{report}");
        assert_eq!(report.data_roots_checked, 2, "{report}");
        assert_eq!(report.pending_checked, 1, "{report}");
    }

    #[test]
    fn convert_round_trip_mdbx_rocks_mdbx() -> eyre::Result<()> {
        let rss_before = vm_kib("VmRSS:");
        let root = temp("convert-round");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let back = root.path().join("db-back");
        let options = options_for(&consensus)?;

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert!(!stats.migrated_v1);
        assert_copy_counts(&stats);
        assert_eq!(
            fs::read_to_string(rocks.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_V3
        );
        assert_equivalent(&verify_mdbx_and_rocks(&mdbx, &rocks, &options)?);
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            assert_v3(&opened, &consensus)?;
        }
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;

        let back_stats = convert_rocks_to_mdbx(&rocks, &back, &options)?;
        assert_copy_counts(&back_stats);
        assert_equivalent(&verify_mdbx_and_rocks(&back, &rocks, &options)?);
        assert_v2(
            &MdbxSubmoduleStore::open(&back, testing_args()?)?,
            &consensus,
        )?;

        let mut replace = options;
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
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut broken = options_for(&consensus)?;
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
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;

        broken.fail_after_batches = None;
        broken.replace_incomplete = true;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &broken)?;
        assert_copy_counts(&stats);
        assert_equivalent(&verify_mdbx_and_rocks(
            &mdbx,
            &rocks,
            &options_for(&consensus)?,
        )?);

        let mdbx_back = root.path().join("db-back");
        broken.fail_after_batches = Some(1);
        broken.replace_incomplete = false;
        let err = convert_rocks_to_mdbx(&rocks, &mdbx_back, &broken).unwrap_err();
        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert_eq!(
            fs::read_to_string(rocks.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_V3
        );
        let open_err = MdbxSubmoduleStore::open(&mdbx_back, testing_args()?).unwrap_err();
        let message = format!("{open_err:#}");
        assert!(message.contains("incomplete"), "{message}");
        assert!(message.contains("submodule-index-convert"), "{message}");

        broken.fail_after_batches = None;
        broken.replace_incomplete = true;
        let stats = convert_rocks_to_mdbx(&rocks, &mdbx_back, &broken)?;
        assert_copy_counts(&stats);
        assert_equivalent(&verify_mdbx_and_rocks(&mdbx_back, &rocks, &broken)?);
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx_back, testing_args()?)?,
            &consensus,
        )?;
        Ok(())
    }

    #[test]
    fn convert_refuses_same_path_and_non_empty_target() -> eyre::Result<()> {
        let root = temp("convert-refuse-target");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;
        let err = convert_mdbx_to_rocks(&mdbx, &mdbx, &options).unwrap_err();
        assert!(format!("{err:#}").contains("same path"), "{err:#}");
        fs::create_dir_all(&rocks)?;
        fs::write(rocks.join("note"), b"keep")?;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options).unwrap_err();
        assert!(
            format!("{err:#}").contains("--replace-incomplete"),
            "{err:#}"
        );
        assert_eq!(fs::read(rocks.join("note"))?, b"keep");
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;
        Ok(())
    }

    #[test]
    fn verify_reports_a_row_only_one_engine_has() -> eyre::Result<()> {
        let root = temp("convert-mismatch");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened
                .update(|tx| tx.add_full_data_path(PartitionChunkOffset(7), b"extra".to_vec()))?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.mismatch_data_path >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn verify_counts_a_derived_path_mismatch() -> eyre::Result<()> {
        let root = temp("convert-verify-path");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
            let hash = store.view(|tx| {
                Ok(tx
                    .tx_interval_at(PartitionChunkOffset(0))?
                    .expect("interval")
                    .tx_path_hash)
            })?;
            store.update(|tx| tx.add_full_tx_path(hash, b"changed".to_vec()))?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.tx_paths_mismatches >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn convert_aborts_when_stored_bytes_differ() -> eyre::Result<()> {
        let root = temp("convert-abort-bytes");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        {
            let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
            let hash = store.view(|tx| {
                Ok(tx
                    .tx_interval_at(PartitionChunkOffset(0))?
                    .expect("interval")
                    .tx_path_hash)
            })?;
            store.update(|tx| tx.add_full_tx_path(hash, b"not-the-derived-path".to_vec()))?;
        }
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("partition offset"), "{message}");
        assert!(!rocks.join("SCHEMA").exists());
        Ok(())
    }

    #[test]
    fn convert_migrates_v1_before_copy() -> eyre::Result<()> {
        use crate::submodule::tables::v1_fixture::master;
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let root = temp("convert-v1");
        let consensus = root.path().join("consensus");
        write_chain(&consensus)?;
        let pair = derived_pair(&consensus)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let data = b"v1-data".to_vec();
        let data_hash = H256::from(hash_sha256(&data));
        let path_hash = tx_path_hash(&pair.proof_a);
        let db = open_or_create_db(&mdbx, master::MasterV1Tables::ALL, testing_args()?)?;
        {
            let tx = db.tx_mut()?;
            tx.put::<master::ChunkPathHashesByOffset>(
                PartitionChunkOffset(0),
                ChunkPathHashes {
                    data_path_hash: Some(data_hash),
                    tx_path_hash: Some(path_hash),
                },
            )?;
            tx.put::<master::ChunkDataPathByPathHash>(data_hash, data.clone())?;
            tx.put::<master::TxPathByTxPathHash>(path_hash, pair.proof_a.clone())?;
            tx.commit()?;
        }
        drop(db);

        let options = options_for(&consensus)?;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert!(stats.migrated_v1);
        assert_eq!(stats.intervals, 1);
        assert_eq!(stats.tx_leaf_bindings, 0);
        assert_eq!(stats.data_roots, 0);
        assert_eq!(stats.data_paths, 1);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        let opened = RocksSubmoduleStore::open(&rocks)?;
        opened.view(|tx| {
            assert_eq!(tx.get_tx_path_by_offset(PartitionChunkOffset(0))?, None);
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(0))?,
                Some(data)
            );
            let place = tx
                .placement_at(PartitionChunkOffset(0))?
                .expect("placement");
            assert_eq!(place.data_root, pair.root_a);
            assert_eq!(place.block_height, Some(1));
            assert_eq!(place.tx_index, Some(0));
            assert_eq!(place.ledger, Some(DataLedger::Publish.get_id()));
            assert_eq!(place.tx_start_offset, RelativeChunkOffset(0));
            assert_eq!(place.data_size, 0);
            assert_eq!(place.start, PartitionChunkOffset(0));
            assert_eq!(place.end, PartitionChunkOffset(0));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn convert_rocks_v2_into_v3_and_back() -> eyre::Result<()> {
        let root = temp("convert-rocks-v2");
        let consensus = root.path().join("consensus");
        write_chain(&consensus)?;
        let v2 = root.path().join("rocks-v2");
        let v3 = root.path().join("rocks-v3");
        let back = root.path().join("db-back");
        let options = options_for(&consensus)?;
        {
            let store = RocksSubmoduleStore::open_v2_with(&v2, RocksTuning::baseline())?;
            write_index(&store, &consensus)?;
        }
        let err = convert_rocks_to_mdbx(&v2, &back, &options).unwrap_err();
        assert!(format!("{err:#}").contains("rocks-to-v3"), "{err:#}");

        let stats = convert_rocks_v2_to_v3(&v2, &v3, &options)?;
        assert_copy_counts(&stats);
        assert_eq!(fs::read_to_string(v3.join("SCHEMA"))?, SUBMODULE_SCHEMA_V3);
        let report = verify_rocks_v2_and_v3(&v2, &v3, &options)?;
        assert_equivalent(&report);
        assert_v3(&RocksSubmoduleStore::open(&v3)?, &consensus)?;
        assert_v2(&RocksSubmoduleStore::open(&v2)?, &consensus)?;

        let back_stats = convert_rocks_to_mdbx(&v3, &back, &options)?;
        assert_copy_counts(&back_stats);
        assert_equivalent(&verify_mdbx_and_rocks(&back, &v3, &options)?);
        assert_v2(
            &MdbxSubmoduleStore::open(&back, testing_args()?)?,
            &consensus,
        )?;
        Ok(())
    }

    #[test]
    fn convert_refuses_when_an_opener_holds_the_lock() -> eyre::Result<()> {
        if let Ok(kind) = std::env::var("IRYS_CONVERT_LOCK_CHILD") {
            return hold_index_lock(&kind);
        }
        let root = temp("convert-lock");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;

        let ready = root.path().join("mdbx.ready");
        let mut child = spawn_lock_child("mdbx", &mdbx, &ready)?;
        wait_ready(&ready, &mut child)?;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        drop(child);
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;

        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let ready = root.path().join("rocks.ready");
        let mut child = spawn_lock_child("rocks", &rocks, &ready)?;
        wait_ready(&ready, &mut child)?;
        let back = root.path().join("db-back");
        let err = convert_rocks_to_mdbx(&rocks, &back, &options).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        drop(child);
        assert_v3(&RocksSubmoduleStore::open(&rocks)?, &consensus)?;
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
