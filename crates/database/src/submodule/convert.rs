// MDBX-SUBMODULE-INDEX: delete the MDBX conversion and the MDBX verify in this file once every node is on RocksDB v1 and the rollback window is closed. verify_rocks_records compares two RocksDB v1 directories and is not an MDBX path.
//! Offline copy between an MDBX submodule index and RocksDB v1.
//!
//! The source is never modified. The completion marker (`rocks/SCHEMA`, or the
//! MDBX schema row) is written last. A crash before that marker leaves
//! `CONVERTING` and no marker. `--replace-incomplete` deletes only a directory
//! that contains `CONVERTING` and none of the protected names.
//!
//! The RocksDB target disables the WAL. The seal flush, then the SCHEMA file,
//! makes that copy durable. An MDBX target commits each batch in durable sync
//! mode and keeps the schema row incomplete until the last commit.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::WrapErr as _;
use irys_types::{
    ChunkDataPath, DataLedger, DataTransactionHeader, H256, PartitionChunkOffset,
    RelativeChunkOffset, TxPath, TxPathHash, hash_sha256,
};
use reth_db::cursor::DbCursorRO as _;
use reth_db::mdbx::DatabaseArguments;
use reth_db::transaction::DbTx as _;
use reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind, DatabaseError};

use crate::database_schema_version;
use crate::db::IrysDatabaseExt as _;
use crate::metadata::MetadataKey;
use crate::submodule::db::read_schema_bytes;
use crate::tx_header_by_txid;
use crate::tx_path_derive::{
    DerivedTxPath, TxPathBlockCache, consensus_is_behind, derive_tx_path_at_offset_cached,
    ledger_frontier, tx_header_schema, tx_path_hash,
};

use super::migrate::{self, OpenRun};
use super::rocks::{self, RocksSubmoduleStore};
use super::store::{MdbxSubmoduleStore, SubmoduleStore, SubmoduleWrite};
use super::tables::v1::ChunkDataPathByPathHash;
use super::tables::{
    ChunkDataPathByOffset, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfo, DataRootInfos,
    DataRootInfosByDataRoot, Metadata, OffsetMeta, PendingBodyMigration,
    PendingBodyMigrationsByOffset, SUBMODULE_CONVERT_INCOMPLETE, SUBMODULE_SCHEMA,
    SUBMODULE_SCHEMA_ROCKS_V1, TxLeafBinding, TxLeafBindingByTxPathHash, TxPathByTxPathHash,
    TxPathInterval, TxPathIntervalByStart,
};

const CONVERTING_FILE: &str = "CONVERTING";
const FORBIDDEN_NAMES: &[&str] = &[
    "chunks.dat",
    "chunks.crc",
    "packing_params.toml",
    "intervals.json",
];

const VERIFY_KEYS: usize = 256;
const SAMPLE_LIMIT: usize = 20;

/// One differing read. `at` is the offset or the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineMismatch {
    pub kind: &'static str,
    pub at: String,
}

/// Keys in one column family whose stored bytes differ, or that exist on only one side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FamilyMismatch {
    pub family: String,
    pub mismatches: u64,
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
    /// Parseable proof leaves plus reverse rows examined on the RocksDB v1 side.
    pub content_offsets_checked: u64,
    /// A proof leaf with no reverse row, or a reverse row whose proof leaf differs.
    pub content_offset_mismatches: u64,
    /// Rows with no covering interval. Not a mismatch: the copy omits them, and
    /// `audit_index` rejects a proof or pending job left with no interval.
    pub skipped_orphans: u64,
    /// Placements past the consensus frontier. The node has not indexed them yet.
    /// A non-zero count is a mismatch: verify must not accept a partial index.
    pub skipped_frontier: u64,
    /// RocksDB v1 rows `audit_index` rejected. Zero on MDBX and schema v2.
    pub index_audit_findings: u64,
    /// RocksDB v1 checksum that is not the value verify expected.
    pub mismatch_chunk_crc: u64,
    /// RocksDB checksums a rollback copy does not carry (`rollback` verify only).
    pub dropped_checksums: u64,
    /// Byte mismatches per column family. Every on-disk family is listed, including a zero count.
    pub family_mismatches: Vec<FamilyMismatch>,
    pub samples: Vec<EngineMismatch>,
}

fn write_labeled(
    f: &mut std::fmt::Formatter<'_>,
    rows: &[(&str, &dyn std::fmt::Display)],
) -> std::fmt::Result {
    for (label, value) in rows {
        writeln!(f, "{label}: {value}")?;
    }
    Ok(())
}

impl EngineEquivalenceReport {
    pub fn has_mismatch(&self) -> bool {
        let counts = [
            self.offset_mismatches,
            self.intervals_mismatches,
            self.tx_paths_mismatches,
            self.tx_leaf_bindings_mismatches,
            self.data_roots_mismatches,
            self.pending_mismatches,
            self.reset_marker_mismatch,
            self.schema_mismatch,
            self.content_offset_mismatches,
            self.mismatch_data_path_hash,
            self.mismatch_tx_path_hash,
            self.skipped_frontier,
            self.index_audit_findings,
            self.mismatch_chunk_crc,
        ];
        counts.into_iter().any(|count| count > 0)
            || self.family_mismatches.iter().any(|row| row.mismatches > 0)
    }
}

impl std::fmt::Display for EngineEquivalenceReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_labeled(
            f,
            &[
                ("offsets_checked", &self.offsets_checked),
                ("offset_mismatches", &self.offset_mismatches),
                ("mismatch_data_path", &self.mismatch_data_path),
                ("mismatch_tx_path", &self.mismatch_tx_path),
                ("mismatch_data_path_hash", &self.mismatch_data_path_hash),
                ("mismatch_tx_path_hash", &self.mismatch_tx_path_hash),
                ("intervals_checked", &self.intervals_checked),
                ("intervals_mismatches", &self.intervals_mismatches),
                ("TxPathByTxPathHash_checked", &self.tx_paths_checked),
                ("TxPathByTxPathHash_mismatches", &self.tx_paths_mismatches),
                (
                    "TxLeafBindingByTxPathHash_checked",
                    &self.tx_leaf_bindings_checked,
                ),
                (
                    "TxLeafBindingByTxPathHash_mismatches",
                    &self.tx_leaf_bindings_mismatches,
                ),
                ("DataRootInfosByDataRoot_checked", &self.data_roots_checked),
                (
                    "DataRootInfosByDataRoot_mismatches",
                    &self.data_roots_mismatches,
                ),
                (
                    "PendingBodyMigrationsByOffset_checked",
                    &self.pending_checked,
                ),
                (
                    "PendingBodyMigrationsByOffset_mismatches",
                    &self.pending_mismatches,
                ),
                ("reset_marker_mismatch", &self.reset_marker_mismatch),
                ("schema_mismatch", &self.schema_mismatch),
                ("content_offsets_checked", &self.content_offsets_checked),
                ("content_offset_mismatches", &self.content_offset_mismatches),
                ("skipped_orphans", &self.skipped_orphans),
                ("skipped_frontier", &self.skipped_frontier),
                ("index_audit_findings", &self.index_audit_findings),
                ("mismatch_chunk_crc", &self.mismatch_chunk_crc),
                ("dropped_checksums", &self.dropped_checksums),
            ],
        )?;
        for row in &self.family_mismatches {
            writeln!(f, "family {}: {}", row.family, row.mismatches)?;
        }
        for sample in &self.samples {
            writeln!(f, "mismatch: {} {}", sample.kind, sample.at)?;
        }
        Ok(())
    }
}

/// Rows copied into the target. The completion marker is not a row count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConvertStats {
    /// Always false. This tool does not migrate the source.
    pub migrated_v1: bool,
    /// The source was a schema v1 MDBX index and was read without writing it.
    pub source_v1: bool,
    pub tx_paths: u64,
    pub tx_leaf_bindings: u64,
    pub intervals: u64,
    pub path_hashes: u64,
    pub data_paths: u64,
    pub data_roots: u64,
    pub pending: u64,
    pub reset_marker: bool,
    pub skipped_orphans: u64,
    pub skipped_frontier: u64,
    pub elapsed_ms: u64,
}

impl std::fmt::Display for ConvertStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_labeled(
            f,
            &[
                ("migrated_v1", &self.migrated_v1),
                ("source_v1", &self.source_v1),
                ("tx_paths", &self.tx_paths),
                ("tx_leaf_bindings", &self.tx_leaf_bindings),
                ("intervals", &self.intervals),
                ("path_hashes", &self.path_hashes),
                ("data_paths", &self.data_paths),
                ("data_roots", &self.data_roots),
                ("pending", &self.pending),
                ("reset_marker", &self.reset_marker),
                ("skipped_orphans", &self.skipped_orphans),
                ("skipped_frontier", &self.skipped_frontier),
                ("elapsed_ms", &self.elapsed_ms),
            ],
        )
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
    /// Validate, scan, and report. Do not create or write the target.
    pub dry_run: bool,
    /// `verify` of a `rocks-to-mdbx` copy. MDBX stores no checksums, so a
    /// RocksDB checksum the node wrote after the forward conversion is
    /// counted in `dropped_checksums` instead of as a mismatch.
    // MDBX-SUBMODULE-INDEX: delete this flag with --rollback. It accepts a rocks-to-mdbx copy that dropped checksums.
    pub rollback: bool,
    /// Keep going when a placement is past the consensus frontier.
    /// An empty consensus block index still fails the run.
    pub allow_unresolved: bool,
    /// When set, the RocksDB target stores this packing record before the marker.
    pub packing_record: Option<super::PackingRecord>,
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
            dry_run: false,
            rollback: false,
            allow_unresolved: false,
            packing_record: None,
        }
    }
}

fn seal_rocks_target(dst: RocksSubmoduleStore, options: &ConvertOptions) -> eyre::Result<()> {
    if let Some(record) = options.packing_record {
        dst.bind_packing_identity(None, &record)?;
    }
    dst.seal_and_stamp()
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
    dry_run: bool,
}

impl CopyProgress {
    fn from_options(options: &ConvertOptions) -> Self {
        Self {
            batches: 0,
            fail_after_batches: options.fail_after_batches,
            dry_run: options.dry_run,
        }
    }

    fn note(&mut self) -> eyre::Result<()> {
        if self.dry_run {
            return Ok(());
        }
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

    /// End offset of the last placement, in key order. `None` when the source
    /// has no placement. This is the offset the consensus database must cover.
    fn highest_placement_end(&self) -> eyre::Result<Option<PartitionChunkOffset>>;
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

    fn highest_placement_end(&self) -> eyre::Result<Option<PartitionChunkOffset>> {
        self.highest_interval_end()
    }
}

/// One INFO line per this interval while `--verify` copies and compares.
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(15);

struct RowPace {
    path: PathBuf,
    phase: &'static str,
    total: u64,
    done: u64,
    started: Instant,
    last_log: Instant,
}

thread_local! {
    static ROW_PACE: std::cell::RefCell<Option<RowPace>> = const { std::cell::RefCell::new(None) };
}

/// Clears the progress pace on drop. The converter does not spawn a thread.
struct PaceGuard;

impl PaceGuard {
    fn start(path: &Path, phase: &'static str, total: u64) -> Self {
        ROW_PACE.with(|cell| {
            *cell.borrow_mut() = Some(RowPace {
                path: path.to_path_buf(),
                phase,
                total,
                done: 0,
                started: Instant::now(),
                last_log: Instant::now(),
            });
        });
        Self
    }
}

impl Drop for PaceGuard {
    fn drop(&mut self) {
        ROW_PACE.with(|cell| *cell.borrow_mut() = None);
    }
}

fn note_rows(rows: u64) {
    if rows == 0 {
        return;
    }
    ROW_PACE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(pace) = slot.as_mut() else {
            return;
        };
        pace.done = pace.done.saturating_add(rows);
        if pace.last_log.elapsed() < PROGRESS_LOG_INTERVAL {
            return;
        }
        let elapsed_ms = u64::try_from(pace.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            path = %pace.path.display(),
            phase = pace.phase,
            rows_done = pace.done,
            rows_total = pace.total,
            percent = migrate::percent_done(pace.done, pace.total),
            rows_per_sec = migrate::per_sec(pace.done, elapsed_ms),
            eta_ms = migrate::eta_ms(pace.done, pace.total, elapsed_ms),
            "submodule index progress"
        );
        pace.last_log = Instant::now();
    });
}

struct ConvertPrep {
    mdbx_dir: PathBuf,
    rocks_dir: PathBuf,
    started: Instant,
    map: PlacementMap,
    db: DatabaseEnv,
}

fn prepare_convert(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertPrep> {
    let mdbx_dir = lexical_target(mdbx_dir)?;
    let rocks_dir = lexical_target(rocks_dir)?;
    let started = Instant::now();
    guard_paths(&mdbx_dir, &rocks_dir, options)?;
    let map = require_map(options)?;
    let db = open_consensus(options)?;
    Ok(ConvertPrep {
        mdbx_dir,
        rocks_dir,
        started,
        map,
        db,
    })
}

fn require_mdbx_dat(mdbx_dir: &Path) -> eyre::Result<()> {
    if !mdbx_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "source {} has no mdbx.dat. Pass the submodule db/ directory.",
            mdbx_dir.display()
        );
    }
    Ok(())
}

/// Copies a v1 or v2 MDBX submodule index into a new RocksDB v1 directory.
///
/// The source is opened read-only. A v1 index is converted from its own rows.
/// It is not migrated. Each stored tx path must match the path derived from
/// the consensus database. A mismatch aborts and names the partition offset.
/// The RocksDB `SCHEMA` file is written only after the rows are durable.
// MDBX-SUBMODULE-INDEX: delete this function with the mdbx-to-rocks command.
pub fn convert_mdbx_to_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let prep = prepare_convert(mdbx_dir, rocks_dir, options)?;
    require_mdbx_dat(&prep.mdbx_dir)?;
    let opened = map_open(open_mdbx_source(&prep.mdbx_dir, exclusive_args(options)))?;
    convert_opened(
        &opened,
        &prep.rocks_dir,
        options,
        &prep.db,
        &prep.map,
        prep.started,
    )
}

/// Convert, then verify, against one open MDBX source.
///
/// The source is warmed and pinned for both passes, so verify re-reads it from
/// memory instead of a cold HDD. The convert's own row checks replace a separate
/// `--dry-run` pass; a failure leaves the same unsealed target a plain convert
/// leaves. The pin drops when this returns.
// MDBX-SUBMODULE-INDEX: delete this function with the mdbx-to-rocks command.
pub fn convert_mdbx_to_rocks_and_verify(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<(ConvertStats, EngineEquivalenceReport)> {
    if options.dry_run {
        eyre::bail!("--dry-run is not valid with --verify");
    }
    let prep = prepare_convert(mdbx_dir, rocks_dir, options)?;
    require_mdbx_dat(&prep.mdbx_dir)?;
    let opened = map_open(open_mdbx_source(&prep.mdbx_dir, exclusive_args(options)))?;
    let _pin = super::warmup::pre_read_index_file(opened_env(&opened), &prep.mdbx_dir);
    let total = source_row_total(&opened)?;
    let stats = {
        let _pace = PaceGuard::start(&prep.mdbx_dir, "convert", total);
        convert_opened(
            &opened,
            &prep.rocks_dir,
            options,
            &prep.db,
            &prep.map,
            prep.started,
        )?
    };
    let report = {
        let _pace = PaceGuard::start(&prep.mdbx_dir, "verify", total);
        verify_opened(
            &opened,
            &prep.rocks_dir,
            &prep.db,
            &prep.map,
            options.rollback,
        )?
    };
    Ok((stats, report))
}

fn convert_opened(
    opened: &OpenedMdbx,
    rocks_dir: &Path,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
    started: Instant,
) -> eyre::Result<ConvertStats> {
    let source_v1 = matches!(opened, OpenedMdbx::V1(_));
    if source_reset(opened)? {
        return Ok(finish_stats(
            write_reset_rocks(rocks_dir, options, source_v1)?,
            started,
            source_v1,
        ));
    }
    let stats = match opened {
        OpenedMdbx::V2(src) => copy_v2_rocks(src, rocks_dir, options, db, map)?,
        OpenedMdbx::V1(env) => copy_v1_rocks(env, rocks_dir, options, db, map)?,
        OpenedMdbx::Empty(_) => write_empty_rocks(rocks_dir, options)?,
    };
    Ok(finish_stats(stats, started, source_v1))
}

/// Compares two RocksDB v1 indexes.
///
/// Every column family is compared as stored bytes. That includes the
/// metadata keys for the schema, the partition, the packing record, and any
/// other metadata key that is present, such as feature flags. Offset proofs
/// and checksums are also counted on their own.
pub fn verify_rocks_records(
    left_dir: &Path,
    right_dir: &Path,
) -> eyre::Result<EngineEquivalenceReport> {
    let left_dir = lexical_target(left_dir)?;
    let right_dir = lexical_target(right_dir)?;
    let left_dir = left_dir.as_path();
    let right_dir = right_dir.as_path();
    require_rocks_v1(left_dir)?;
    require_rocks_v1(right_dir)?;
    let left = map_open(RocksSubmoduleStore::open_read_only(left_dir))?;
    let right = map_open(RocksSubmoduleStore::open_read_only(right_dir))?;
    verify_wrapped_records(&left, &right)
}

fn verify_wrapped_records(
    left: &RocksSubmoduleStore,
    right: &RocksSubmoduleStore,
) -> eyre::Result<EngineEquivalenceReport> {
    let mut report = EngineEquivalenceReport::default();
    if left.reset_in_progress()? != right.reset_in_progress()? {
        report.reset_marker_mismatch = 1;
        note(&mut report, "reset_marker", "metadata".to_owned());
    }
    merge_keys(
        &mut |after| left.read_offset_row_keys(after, VERIFY_KEYS),
        &mut |after| right.read_offset_row_keys(after, VERIFY_KEYS),
        |keys| {
            let left_rows = left.view(|tx| {
                keys.iter()
                    .map(|key| tx.get_offset_meta(*key))
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            let right_rows = right.view(|tx| {
                keys.iter()
                    .map(|key| tx.get_offset_meta(*key))
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            for (index, key) in keys.iter().enumerate() {
                report.offsets_checked = report.offsets_checked.saturating_add(1);
                let left_proof = left_rows[index]
                    .as_ref()
                    .and_then(|meta| meta.proof.clone());
                let right_proof = right_rows[index]
                    .as_ref()
                    .and_then(|meta| meta.proof.clone());
                if left_proof != right_proof {
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                    report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                    note(&mut report, "data_path", format!("offset {}", key.0));
                }
                let left_crc = left_rows[index].as_ref().and_then(|meta| meta.crc);
                let right_crc = right_rows[index].as_ref().and_then(|meta| meta.crc);
                if left_crc != right_crc {
                    report.mismatch_chunk_crc = report.mismatch_chunk_crc.saturating_add(1);
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                    note(&mut report, "chunk_crc", format!("offset {}", key.0));
                }
            }
            Ok(())
        },
    )?;
    compare_column_families(left, right, &mut report)?;
    note_index_audit(right, &mut report)?;
    Ok(report)
}

/// Byte-compare every on-disk column family, including an empty one.
///
/// A metadata key is part of the `Metadata` family. Schema, partition,
/// packing, and a feature-flags row are compared when stored and match when
/// both sides omit the key.
fn compare_column_families(
    left: &RocksSubmoduleStore,
    right: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut names = BTreeSet::new();
    names.extend(left.column_family_names()?);
    names.extend(right.column_family_names()?);
    for name in names {
        let mismatches = count_family_mismatches(left, right, &name)?;
        if mismatches > 0 {
            note(report, "family", format!("{name} {mismatches}"));
        }
        report.family_mismatches.push(FamilyMismatch {
            family: name,
            mismatches,
        });
    }
    Ok(())
}

struct RawPage {
    rows: Vec<(Vec<u8>, Vec<u8>)>,
    index: usize,
}

impl RawPage {
    fn load(store: &RocksSubmoduleStore, name: &str, after: Option<&[u8]>) -> eyre::Result<Self> {
        Ok(Self {
            rows: store.read_raw_cf_page(name, after, VERIFY_KEYS)?,
            index: 0,
        })
    }

    fn current(&self) -> Option<&(Vec<u8>, Vec<u8>)> {
        self.rows.get(self.index)
    }

    fn advance(&mut self, store: &RocksSubmoduleStore, name: &str) -> eyre::Result<()> {
        self.index = self.index.saturating_add(1);
        if self.index < self.rows.len() {
            return Ok(());
        }
        let Some(after) = self.rows.last().map(|(key, _)| key.clone()) else {
            return Ok(());
        };
        self.rows = store.read_raw_cf_page(name, Some(&after), VERIFY_KEYS)?;
        self.index = 0;
        Ok(())
    }
}

fn count_family_mismatches(
    left_store: &RocksSubmoduleStore,
    right_store: &RocksSubmoduleStore,
    name: &str,
) -> eyre::Result<u64> {
    let mut left = RawPage::load(left_store, name, None)?;
    let mut right = RawPage::load(right_store, name, None)?;
    let mut mismatches = 0_u64;
    loop {
        enum Side {
            Same,
            Left,
            Right,
            Value,
        }
        let side = match (left.current(), right.current()) {
            (None, None) => break,
            (Some(_), None) => Side::Left,
            (None, Some(_)) => Side::Right,
            (Some((left_key, left_value)), Some((right_key, right_value))) => {
                match left_key.cmp(right_key) {
                    std::cmp::Ordering::Less => Side::Left,
                    std::cmp::Ordering::Greater => Side::Right,
                    std::cmp::Ordering::Equal if left_value != right_value => Side::Value,
                    std::cmp::Ordering::Equal => Side::Same,
                }
            }
        };
        if !matches!(side, Side::Same) {
            mismatches = mismatches.saturating_add(1);
        }
        match side {
            Side::Same | Side::Value => {
                left.advance(left_store, name)?;
                right.advance(right_store, name)?;
            }
            Side::Left => left.advance(left_store, name)?,
            Side::Right => right.advance(right_store, name)?,
        }
    }
    Ok(mismatches)
}

/// Rebuilds an MDBX schema v2 index from a RocksDB v1 index.
///
/// Tx paths and leaf bindings are derived from the consensus database. A
/// stored height, tx index, or data root that disagrees with that derive
/// aborts the run and names the partition offset. The MDBX schema row stays
/// [`SUBMODULE_CONVERT_INCOMPLETE`] until every row is durable, then becomes
/// [`SUBMODULE_SCHEMA`].
// MDBX-SUBMODULE-INDEX: delete this function with the rocks-to-mdbx command once the rollback window is closed.
pub fn convert_rocks_to_mdbx(
    rocks_dir: &Path,
    mdbx_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let rocks_dir = lexical_target(rocks_dir)?;
    let mdbx_dir = lexical_target(mdbx_dir)?;
    let rocks_dir = rocks_dir.as_path();
    let mdbx_dir = mdbx_dir.as_path();
    let started = Instant::now();
    guard_paths(rocks_dir, mdbx_dir, options)?;
    let map = require_map(options)?;
    let db = open_consensus(options)?;
    require_rocks_v1(rocks_dir)?;
    let src = map_open(RocksSubmoduleStore::open_read_only(rocks_dir))?;
    if src.reset_in_progress()? {
        return Ok(finish_stats(
            write_reset_mdbx(mdbx_dir, options)?,
            started,
            false,
        ));
    }
    ensure_consensus_covers(
        &db,
        &map,
        src.highest_placement_end()?,
        options.allow_unresolved,
    )?;
    let stats = if options.dry_run {
        let stats = copy_rocks_into_mdbx(&src, None, options, &db, &map)?;
        refuse_unresolved(&stats, options)?;
        stats
    } else {
        let args = exclusive_args(options);
        // Held until the completion marker is durable. Drop releases `LOCK`.
        let _lock = prepare_mdbx_target(mdbx_dir, args.clone(), options.replace_incomplete)?;
        let dst = map_open(MdbxSubmoduleStore::open_for_convert(mdbx_dir, args))?;
        let stats = copy_rocks_into_mdbx(&src, Some(&dst), options, &db, &map)?;
        refuse_unresolved(&stats, options)?;
        dst.put_schema_bytes(SUBMODULE_SCHEMA.as_bytes())?;
        clear_converting_sentinel(mdbx_dir)?;
        stats
    };
    Ok(finish_stats(stats, started, false))
}

/// Compares a schema v2 MDBX index with a RocksDB v1 RocksDB index.
///
/// Data paths, pending jobs, and placement fields are read from both engines.
/// The tx path is the stored v2 bytes on one side and the path derived from
/// the consensus database on the other. A checksum on a row whose source has
/// none, and a checksum-only target row the source does not have, are
/// mismatches. Exit the process when
/// [`EngineEquivalenceReport::has_mismatch`] is set.
///
/// Does not migrate and does not write. A v1 MDBX directory is read as v1.
// MDBX-SUBMODULE-INDEX: delete this function with verify of an MDBX source. verify_rocks_records is the Rocks-only compare.
pub fn verify_mdbx_and_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<EngineEquivalenceReport> {
    if options.dry_run {
        eyre::bail!("--dry-run is not valid for verify");
    }
    let mdbx_dir = lexical_target(mdbx_dir)?;
    let rocks_dir = lexical_target(rocks_dir)?;
    let mdbx_dir = mdbx_dir.as_path();
    let rocks_dir = rocks_dir.as_path();
    guard_paths(mdbx_dir, rocks_dir, options)?;
    let map = require_map(options)?;
    let db = open_consensus(options)?;
    require_rocks_v1(rocks_dir)?;
    let opened = map_open(open_mdbx_source(mdbx_dir, exclusive_args(options)))?;
    let rocks = map_open(RocksSubmoduleStore::open_read_only(rocks_dir))?;
    verify_opened_store(&opened, &rocks, &db, &map, options.rollback)
}

fn verify_opened(
    opened: &OpenedMdbx,
    rocks_dir: &Path,
    db: &DatabaseEnv,
    map: &PlacementMap,
    rollback: bool,
) -> eyre::Result<EngineEquivalenceReport> {
    require_rocks_v1(rocks_dir)?;
    let rocks = map_open(RocksSubmoduleStore::open_read_only(rocks_dir))?;
    verify_opened_store(opened, &rocks, db, map, rollback)
}

fn verify_opened_store(
    opened: &OpenedMdbx,
    rocks: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    map: &PlacementMap,
    rollback: bool,
) -> eyre::Result<EngineEquivalenceReport> {
    match opened {
        OpenedMdbx::V2(mdbx) => {
            if mdbx.reset_in_progress()? {
                verify_reset_target(true, rocks)
            } else {
                let schema = mdbx.schema_bytes()?;
                verify_mdbx_rows_against_rocks(mdbx, rocks, schema, db, map, rollback)
            }
        }
        OpenedMdbx::V1(env) => {
            if reset_marker_on(env)? {
                verify_reset_target(true, rocks)
            } else {
                verify_mdbx_v1_against_rocks(env, rocks, db, map)
            }
        }
        OpenedMdbx::Empty(env) => verify_empty_and_rocks(env, rocks),
    }
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
            "ledger id {} is not a data ledger. Publish is 0, Submit is 1, OneYear is 10, ThirtyDay is 20.",
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
            "consensus database {} has no mdbx.dat. Pass <node>/.irys/irys_consensus_data (config.toml base_directory plus irys_consensus_data).",
            path.display()
        );
    }
    let args = options.consensus.clone().with_exclusive(Some(false));
    let db = DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .map_err(|err| eyre::eyre!("read-only open {}: {err}", path.display()))?;
    let version = db
        .view(database_schema_version)
        .map_err(|err| eyre::eyre!("schema txn failed: {err}"))?
        .map_err(|err| eyre::eyre!("read consensus schema stamp: {err}"))?;
    let schema = tx_header_schema(version);
    if !schema.readable {
        eyre::bail!("{}", schema.message);
    }
    Ok(db)
}

/// Orphan and frontier skips for rows that follow a placement pass.
struct SkipCounts<'a> {
    skips: &'a FrontierSkips,
    orphans: &'a mut u64,
    frontier: &'a mut u64,
}

impl SkipCounts<'_> {
    /// `false` when the row is outside every written placement, or inside a frontier skip.
    fn keep(&mut self, offset: u32, covered: bool) -> bool {
        if !covered {
            *self.orphans = self.orphans.saturating_add(1);
            return false;
        }
        if self.skips.covers(offset) {
            *self.frontier = self.frontier.saturating_add(1);
            return false;
        }
        true
    }
}

fn copy_covered_rows<S, T>(
    src: &S,
    dst: Option<&RocksSubmoduleStore>,
    progress: &mut CopyProgress,
    skip: &mut SkipCounts<'_>,
    mut read: impl FnMut(Option<PartitionChunkOffset>) -> eyre::Result<Vec<(PartitionChunkOffset, T)>>,
    mut write: impl FnMut(&mut dyn SubmoduleWrite, PartitionChunkOffset, T) -> eyre::Result<()>,
    kind: &'static str,
) -> eyre::Result<u64>
where
    S: SubmoduleStore,
{
    copy_rows(
        &mut read,
        |batch| {
            // One read transaction for the batch. A per-row view is one
            // transaction per chunk on a full partition.
            let covered = src.view(|tx| {
                batch
                    .iter()
                    .map(|(offset, _)| Ok(tx.tx_interval_at(*offset)?.is_some()))
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            let mut kept = Vec::new();
            for ((offset, value), covered) in batch.into_iter().zip(covered) {
                if !skip.keep(offset.0, covered) {
                    continue;
                }
                kept.push((offset, value));
            }
            let copied = u64::try_from(kept.len()).unwrap_or(u64::MAX);
            if kept.is_empty() {
                return Ok(0);
            }
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for (offset, value) in kept {
                        require_interval(tx, offset, kind)?;
                        write(tx, offset, value)?;
                    }
                    Ok(())
                })?;
            }
            Ok(copied)
        },
        progress,
    )
}

fn copy_mdbx_into_rocks<S>(
    src: &S,
    dst: Option<&RocksSubmoduleStore>,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats>
where
    S: RowSource + SubmoduleStore,
{
    let mut progress = CopyProgress::from_options(options);
    if !options.dry_run && progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limits = Limits::from_options(options);
    let mut cache = TxPathBlockCache::new();
    let mut skips = FrontierSkips::default();
    let mut bindings = 0_u64;
    let mut infos = 0_u64;
    let mut skipped_frontier = 0_u64;
    let intervals = copy_rows(
        |after| src.read_intervals(after, limits),
        |batch| {
            let mut prepared = Vec::with_capacity(batch.len());
            for (start, interval) in &batch {
                let run = prepare_v2_placement(src, db, &mut cache, map, *start, interval)?;
                for (from, to) in run.frontier {
                    skipped_frontier = skipped_frontier.saturating_add(1);
                    skips.push(from, to);
                }
                for item in run.placements {
                    if item.had_binding {
                        bindings = bindings.saturating_add(1);
                    }
                    if item.info.is_some() {
                        infos = infos.saturating_add(1);
                    }
                    prepared.push(item);
                }
            }
            let copied = u64::try_from(prepared.len()).unwrap_or(u64::MAX);
            if prepared.is_empty() {
                return Ok(0);
            }
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for item in &prepared {
                        write_rocks_placement(tx, item)?;
                    }
                    Ok(())
                })?;
            }
            Ok(copied)
        },
        &mut progress,
    )?;
    let mut skipped_orphans = 0_u64;
    let (data_paths, pending) = {
        let mut skip = SkipCounts {
            skips: &skips,
            orphans: &mut skipped_orphans,
            frontier: &mut skipped_frontier,
        };
        let data_paths = copy_covered_rows(
            src,
            dst,
            &mut progress,
            &mut skip,
            |after| src.read_data_paths(after, limits),
            |tx, offset, path| tx.add_full_data_path(offset, path),
            "data path",
        )?;
        let pending = copy_covered_rows(
            src,
            dst,
            &mut progress,
            &mut skip,
            |after| src.read_pending(after, limits),
            |tx, offset, job| tx.add_pending_body_migration(offset, &job),
            "pending body migration",
        )?;
        (data_paths, pending)
    };
    Ok(ConvertStats {
        tx_paths: intervals,
        tx_leaf_bindings: bindings,
        intervals,
        path_hashes: data_paths,
        data_paths,
        data_roots: infos,
        pending,
        skipped_orphans,
        skipped_frontier,
        ..Default::default()
    })
}

fn copy_rocks_into_mdbx(
    src: &RocksSubmoduleStore,
    dst: Option<&MdbxSubmoduleStore>,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats> {
    let mut progress = CopyProgress::from_options(options);
    if !options.dry_run && progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limit = options.batch_rows.max(1);
    let mut cache = TxPathBlockCache::new();
    let mut skips = FrontierSkips::default();
    let mut after = None;
    let mut intervals = 0_u64;
    let mut infos = 0_u64;
    let mut skipped_frontier = 0_u64;
    loop {
        let batch = src.read_placements(after, limit)?;
        let Some(last) = batch.last().map(|row| row.start) else {
            break;
        };
        let mut rebuilt = Vec::with_capacity(batch.len());
        for placement in &batch {
            match rebuild_from_placement(db, &mut cache, map, placement)? {
                Some(item) => {
                    if item.info.is_some() {
                        infos = infos.saturating_add(1);
                    }
                    rebuilt.push(item);
                }
                None => {
                    skipped_frontier = skipped_frontier.saturating_add(1);
                    skips.push(placement.start, placement.end);
                }
            }
        }
        let copied = u64::try_from(rebuilt.len()).unwrap_or(u64::MAX);
        if copied > 0 {
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for item in &rebuilt {
                        write_v2_placement(tx, item)?;
                    }
                    Ok(())
                })?;
            }
            progress.note()?;
        }
        intervals = intervals.saturating_add(copied);
        after = Some(last);
    }
    let mut skipped_orphans = 0_u64;
    let mut skip = SkipCounts {
        skips: &skips,
        orphans: &mut skipped_orphans,
        frontier: &mut skipped_frontier,
    };
    let data_paths = copy_keyed(
        |after| src.read_offset_row_keys(after, limit),
        |keys| {
            let rows = src.view(|tx| {
                keys.iter()
                    .map(|offset| {
                        let path = tx.get_data_path_by_offset(*offset)?;
                        let covered = tx.placement_at(*offset)?.is_some();
                        Ok((*offset, path, covered))
                    })
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            let mut kept = Vec::new();
            for (offset, path, covered) in rows {
                let Some(path) = path else {
                    // Checksum-only entropy row. MDBX stores proofs only.
                    continue;
                };
                if !skip.keep(offset.0, covered) {
                    continue;
                }
                kept.push((offset, path));
            }
            let copied = u64::try_from(kept.len()).unwrap_or(u64::MAX);
            if kept.is_empty() {
                return Ok(0);
            }
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for (offset, path) in kept {
                        require_interval(tx, offset, "data path")?;
                        let hash = H256::from(hash_sha256(&path));
                        tx.add_full_data_path(offset, path)?;
                        tx.add_data_path_hash_to_offset_index(offset, Some(hash))?;
                    }
                    Ok(())
                })?;
            }
            Ok(copied)
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
                        let covered = tx.placement_at(*offset)?.is_some();
                        Ok((*offset, job, covered))
                    })
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            let mut kept = Vec::new();
            for (offset, job, covered) in rows {
                let Some(job) = job else {
                    eyre::bail!("pending key {} has no job", offset.0);
                };
                if !skip.keep(offset.0, covered) {
                    continue;
                }
                kept.push((offset, job));
            }
            let copied = u64::try_from(kept.len()).unwrap_or(u64::MAX);
            if kept.is_empty() {
                return Ok(0);
            }
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for (offset, job) in &kept {
                        require_interval(tx, *offset, "pending body migration")?;
                        tx.add_pending_body_migration(*offset, job)?;
                    }
                    Ok(())
                })?;
            }
            Ok(copied)
        },
        &mut progress,
    )?;
    Ok(ConvertStats {
        tx_paths: intervals,
        tx_leaf_bindings: intervals,
        intervals,
        path_hashes: data_paths,
        data_paths,
        data_roots: infos,
        pending,
        skipped_orphans,
        skipped_frontier,
        ..Default::default()
    })
}

fn copy_keyed<K>(
    mut read: impl FnMut(Option<K>) -> eyre::Result<Vec<K>>,
    mut write: impl FnMut(&[K]) -> eyre::Result<u64>,
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
        let n = write(&batch)?;
        copied = copied.saturating_add(n);
        if n > 0 {
            progress.note()?;
        }
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

struct PreparedRun {
    placements: Vec<PreparedPlacement>,
    /// Inclusive ranges past the consensus frontier. The prefix is still written.
    frontier: Vec<(PartitionChunkOffset, PartitionChunkOffset)>,
}

struct SplitInput<'a> {
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    stored: &'a [u8],
    path_hash: TxPathHash,
    binding: Option<TxLeafBinding>,
}

fn prepare_v2_placement<S: SubmoduleStore>(
    src: &S,
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    start: PartitionChunkOffset,
    interval: &TxPathInterval,
) -> eyre::Result<PreparedRun> {
    // The binding names the data root the infos belong to. Derive checks that
    // root before the infos are used, so the three reads share one view.
    // A missing binding does not know the root until derive, and reads infos then.
    let (stored, binding, mut infos) = src.view(|tx| {
        let stored = tx.get_full_tx_path(interval.tx_path_hash)?;
        let binding = tx.get_tx_leaf_binding(interval.tx_path_hash)?;
        let infos = match &binding {
            Some(binding) => tx.get_data_root_infos_for_data_root(binding.data_root)?,
            None => None,
        };
        Ok((stored, binding, infos))
    })?;
    let stored =
        stored.ok_or_else(|| eyre::eyre!("partition offset {} has no stored tx path", start.0))?;
    let bound_root = binding.as_ref().map(|binding| binding.data_root);
    split_stored_interval(
        db,
        cache,
        map,
        SplitInput {
            start,
            end: interval.end,
            stored: &stored,
            path_hash: interval.tx_path_hash,
            binding,
        },
        |root| {
            if bound_root == Some(root) {
                return Ok(infos.take().map(|infos| infos.0).unwrap_or_default());
            }
            Ok(src
                .view(|tx| tx.get_data_root_infos_for_data_root(root))?
                .map(|infos| infos.0)
                .unwrap_or_default())
        },
    )
}

/// One RocksDB placement per tx inside a stored interval.
///
/// MDBX merges adjacent offsets that share a tx-path hash. Two txs with
/// the same bytes then occupy one interval. Cuts are the interval start plus
/// every info start strictly inside it, the same cuts a placement read uses.
/// The first segment must equal the stored bytes. Later segments use the
/// derived proof. A later segment whose data root differs is corruption and
/// aborts the run. A later segment the consensus database does not cover is
/// a frontier skip, and the resolved prefix is still returned.
fn split_stored_interval(
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    input: SplitInput<'_>,
    mut load_infos: impl FnMut(H256) -> eyre::Result<Vec<DataRootInfo>>,
) -> eyre::Result<PreparedRun> {
    let SplitInput {
        start,
        end,
        stored,
        path_hash,
        binding,
    } = input;
    let Some(first) = derive_at(db, cache, map, start)? else {
        return Ok(PreparedRun {
            placements: Vec::new(),
            frontier: vec![(start, end)],
        });
    };
    if first.proof.proof.as_slice() != stored {
        let start_ledger = map.ledger_offset(start)?;
        eyre::bail!(
            "tx path at partition offset {} (ledger {} offset {start_ledger}) does not match the derived path",
            start.0,
            map.ledger.get_id()
        );
    }
    let first_header = load_header(db, &first, start)?;
    if let Some(binding) = &binding
        && (binding.data_root != first.data_root || binding.prefix_hash != first_header.prefix_hash)
    {
        eyre::bail!(
            "tx leaf binding at partition offset {} does not match the derived tx",
            start.0
        );
    }
    let infos = load_infos(first.data_root)?;
    let segments = tx_segments(start, end, &infos);
    let mut placements = Vec::with_capacity(segments.len());
    let mut frontier = Vec::new();
    let mut first_header = Some(first_header);
    for (index, (seg_start, seg_end)) in segments.into_iter().enumerate() {
        let (derived, header, seg_hash, had_binding) = if index == 0 {
            let Some(header) = first_header.take() else {
                eyre::bail!("partition offset {} lost its tx header", start.0);
            };
            (first.clone(), header, path_hash, binding.is_some())
        } else {
            let Some(derived) = derive_at(db, cache, map, seg_start)? else {
                frontier.push((seg_start, end));
                break;
            };
            if derived.data_root != first.data_root {
                eyre::bail!(
                    "partition offset {} derives a different data root",
                    seg_start.0
                );
            }
            let header = load_header(db, &derived, seg_start)?;
            let seg_hash = tx_path_hash(&derived.proof.proof);
            (derived, header, seg_hash, true)
        };
        if seg_end != seg_start {
            match derive_at(db, cache, map, seg_end)? {
                None => {
                    frontier.push((seg_start, end));
                    break;
                }
                Some(at_end)
                    if at_end.height != derived.height || at_end.tx_index != derived.tx_index =>
                {
                    eyre::bail!(
                        "partition offset {} through {} covers more than one tx",
                        seg_start.0,
                        seg_end.0
                    );
                }
                Some(_) => {}
            }
        }
        let tx_index = u32::try_from(derived.tx_index)
            .map_err(|_| eyre::eyre!("tx index at partition offset {} exceeds u32", seg_start.0))?;
        placements.push(PreparedPlacement {
            start: seg_start,
            end: seg_end,
            path_hash: seg_hash,
            data_root: derived.data_root,
            prefix_hash: header.prefix_hash,
            info: select_covering_info(&infos, seg_start)?,
            ledger_id: map.ledger.get_id(),
            block_height: derived.height,
            tx_index,
            had_binding,
        });
    }
    Ok(PreparedRun {
        placements,
        frontier,
    })
}

fn load_header(
    db: &DatabaseEnv,
    derived: &DerivedTxPath,
    at: PartitionChunkOffset,
) -> eyre::Result<DataTransactionHeader> {
    db.view_eyre(|tx| tx_header_by_txid(tx, &derived.tx_id))?
        .ok_or_else(|| {
            eyre::eyre!(
                "tx header {} missing for partition offset {}",
                derived.tx_id,
                at.0
            )
        })
}

fn derive_at(
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<DerivedTxPath>> {
    let ledger_offset = map.ledger_offset(offset)?;
    match derive_tx_path_at_offset_cached(db, cache, map.ledger, ledger_offset, map.chunk_size) {
        Ok(derived) => Ok(Some(derived)),
        Err(err) if consensus_is_behind(&err) => Ok(None),
        Err(err) => {
            Err(err).wrap_err_with(|| format!("derive tx path at partition offset {}", offset.0))
        }
    }
}

fn select_covering_info(
    infos: &[DataRootInfo],
    probe: PartitionChunkOffset,
) -> eyre::Result<Option<DataRootInfo>> {
    if infos.is_empty() {
        return Ok(None);
    }
    let mut sorted = infos.to_vec();
    sorted.sort_unstable();
    let probe_rel = RelativeChunkOffset(
        i32::try_from(probe.0)
            .map_err(|_| eyre::eyre!("partition offset {} does not fit in i32", probe.0))?,
    );
    let index = sorted
        .partition_point(|info| info.start_offset <= probe_rel)
        .saturating_sub(1);
    let Some(info) = sorted.get(index) else {
        return Ok(None);
    };
    Ok((info.start_offset <= probe_rel).then(|| info.clone()))
}

/// Inclusive segments of `[start, end]`. A negative info start does not cut.
fn tx_segments(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    infos: &[DataRootInfo],
) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
    let mut cuts = vec![start];
    for info in infos {
        let Ok(at) = u32::try_from(info.start_offset.0) else {
            continue;
        };
        if at > start.0 && at <= end.0 {
            cuts.push(PartitionChunkOffset(at));
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::with_capacity(cuts.len());
    for (index, cut) in cuts.iter().enumerate() {
        let seg_end = match cuts.get(index + 1) {
            Some(next) => PartitionChunkOffset(next.0 - 1),
            None => end,
        };
        out.push((*cut, seg_end));
    }
    out
}

fn write_rocks_placement(
    tx: &mut dyn SubmoduleWrite,
    item: &PreparedPlacement,
) -> eyre::Result<()> {
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
        tx.add_data_root_info_at(item.start, item.data_root, info)?;
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
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    placement: &super::tables::Placement,
) -> eyre::Result<Option<RebuiltPlacement>> {
    if placement.ledger.is_some_and(|id| id != map.ledger.get_id()) {
        eyre::bail!(
            "ledger at partition offset {} does not match --ledger {}",
            placement.start.0,
            map.ledger.get_id()
        );
    }
    let Some(derived) = derive_at(db, cache, map, placement.start)? else {
        return Ok(None);
    };
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
    if placement.end != placement.start {
        let Some(at_end) = derive_at(db, cache, map, placement.end)? else {
            return Ok(None);
        };
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
    Ok(Some(RebuiltPlacement {
        start: placement.start,
        end: placement.end,
        path_hash: tx_path_hash(&derived.proof.proof),
        proof: derived.proof.proof,
        data_root: derived.data_root,
        prefix_hash: header.prefix_hash,
        info,
    }))
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

fn verify_mdbx_rows_against_rocks<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    v2_schema: Option<Vec<u8>>,
    db: &DatabaseEnv,
    map: &PlacementMap,
    rollback: bool,
) -> eyre::Result<EngineEquivalenceReport>
where
    S: RowSource + SubmoduleStore,
{
    let mut report = EngineEquivalenceReport::default();
    verify_data_paths(v2, v3, rollback, &mut report)?;
    verify_placements(v2, v3, db, map, &mut report)?;
    verify_pending_rows(v2, v3, &mut report)?;
    verify_markers(v2, v3, v2_schema, &mut report)?;
    verify_content_index(v3, &mut report)?;
    note_index_audit(v3, &mut report)?;
    Ok(report)
}

fn verify_content_index(
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let audit = v3.audit_content_index()?;
    report.content_offsets_checked = audit.proofs_with_leaf.saturating_add(audit.reverse_rows);
    report.content_offset_mismatches = audit.missing_reverse.saturating_add(audit.dangling_reverse);
    if report.content_offset_mismatches > 0 {
        note(
            report,
            "content_offset",
            format!(
                "missing_reverse={} dangling_reverse={}",
                audit.missing_reverse, audit.dangling_reverse
            ),
        );
    }
    Ok(())
}

fn note_index_audit(
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let findings = v3.audit_index()?;
    report.index_audit_findings = u64::try_from(findings.len()).unwrap_or(u64::MAX);
    for finding in findings.into_iter().take(SAMPLE_LIMIT) {
        note(report, "index_audit", finding);
    }
    Ok(())
}

fn verify_data_paths<S>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    rollback: bool,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()>
where
    S: RowSource + SubmoduleStore,
{
    merge_keys(
        &mut |after| v2.read_offset_keys(OffsetTable::DataPaths, after, VERIFY_KEYS),
        &mut |after| v3.read_offset_row_keys(after, VERIFY_KEYS),
        |keys| {
            note_verify_view();
            let (left, stored_hashes) = v2.view(|tx| {
                let paths = read_data_paths_at(tx, keys)?;
                let hashes = keys
                    .iter()
                    .map(|key| tx.get_path_hashes_by_offset(*key))
                    .collect::<eyre::Result<Vec<_>>>()?;
                Ok((paths, hashes))
            })?;
            let right = v3.view(|tx| read_data_paths_at(tx, keys))?;
            let right_meta = v3.view(|tx| {
                keys.iter()
                    .map(|key| tx.get_offset_meta(*key))
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            let orphan_keys: Vec<_> = keys
                .iter()
                .enumerate()
                .filter(|(index, _)| left[*index].is_some() && right[*index].is_none())
                .map(|(_, key)| *key)
                .collect();
            let orphan_intervals = if orphan_keys.is_empty() {
                Vec::new()
            } else {
                note_verify_view();
                v2.view(|tx| {
                    orphan_keys
                        .iter()
                        .map(|key| tx.tx_interval_at(*key))
                        .collect::<eyre::Result<Vec<_>>>()
                })?
            };
            let mut orphan_at = 0_usize;
            for (index, key) in keys.iter().enumerate() {
                if left[index].is_some() && right[index].is_none() {
                    let interval = orphan_intervals[orphan_at].as_ref();
                    orphan_at = orphan_at.saturating_add(1);
                    if interval.is_none() {
                        report.skipped_orphans = report.skipped_orphans.saturating_add(1);
                        continue;
                    }
                }
                report.offsets_checked = report.offsets_checked.saturating_add(1);
                if left[index] != right[index] {
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                    report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                    note(report, "data_path", format!("offset {}", key.0));
                }
                note_mdbx_checksum(
                    left[index].is_some(),
                    right_meta[index].as_ref(),
                    *key,
                    rollback,
                    report,
                );
                if let Some(path) = &left[index] {
                    let digest = H256::from(hash_sha256(path));
                    let stored = stored_hashes[index]
                        .as_ref()
                        .and_then(|hashes| hashes.data_path_hash);
                    if stored != Some(digest) {
                        report.mismatch_data_path_hash =
                            report.mismatch_data_path_hash.saturating_add(1);
                        note(report, "data_path_hash", format!("offset {}", key.0));
                    }
                }
            }
            Ok(())
        },
    )
}

/// MDBX stores no checksum.
///
/// A checksum on a source row is invented. A checksum-only target row whose
/// source has no proof is not a matching absence.
fn note_mdbx_checksum(
    source_has_proof: bool,
    target: Option<&OffsetMeta>,
    key: PartitionChunkOffset,
    rollback: bool,
    report: &mut EngineEquivalenceReport,
) {
    let Some(target) = target else {
        return;
    };
    if target.crc.is_none() {
        return;
    }
    if rollback {
        // The MDBX side is the copy here, and MDBX has no checksum column.
        report.dropped_checksums = report.dropped_checksums.saturating_add(1);
        return;
    }
    if target.proof.is_none() && !source_has_proof {
        report.mismatch_chunk_crc = report.mismatch_chunk_crc.saturating_add(1);
        report.offset_mismatches = report.offset_mismatches.saturating_add(1);
        note(
            report,
            "chunk_crc",
            format!("offset {} checksum-only row is not in the source", key.0),
        );
        return;
    }
    if source_has_proof {
        report.mismatch_chunk_crc = report.mismatch_chunk_crc.saturating_add(1);
        report.offset_mismatches = report.offset_mismatches.saturating_add(1);
        note(
            report,
            "chunk_crc",
            format!("offset {} checksum is not in the source", key.0),
        );
    }
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
    note_verify_placement_cache();
    let mut cache = TxPathBlockCache::new();
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
                .read_placements(after, VERIFY_KEYS)?
                .into_iter()
                .map(|row| row.start)
                .collect())
        },
        |keys| {
            for key in keys {
                report.intervals_checked = report.intervals_checked.saturating_add(1);
                report.tx_paths_checked = report.tx_paths_checked.saturating_add(1);
                compare_one_placement(v2, v3, db, &mut cache, map, *key, report)?;
            }
            Ok(())
        },
    )
}

fn compare_one_placement<S: SubmoduleStore>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    key: PartitionChunkOffset,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let interval = v2.view(|tx| tx.tx_interval_at(key))?;
    let place = v3.view(|tx| tx.placement_at(key))?;
    let Some(interval) = interval else {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    };
    if place.is_none() && derive_at(db, cache, map, key)?.is_none() {
        report.skipped_frontier = report.skipped_frontier.saturating_add(1);
        return Ok(());
    }
    let Some(place) = place else {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    };
    let binding = v2.view(|tx| tx.get_tx_leaf_binding(interval.tx_path_hash))?;
    let info_root = binding
        .as_ref()
        .map(|item| item.data_root)
        .unwrap_or(place.data_root);
    let infos = v2
        .view(|tx| tx.get_data_root_infos_for_data_root(info_root))?
        .map(|infos| infos.0)
        .unwrap_or_default();
    let segments = tx_segments(interval.start, interval.end, &infos);
    let Some(segment_end) = segments
        .iter()
        .find(|(start, _)| *start == key)
        .map(|(_, end)| *end)
    else {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    };
    if place.start != key || place.end != segment_end {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
        return Ok(());
    }
    let is_first = key == interval.start;
    let stored = if is_first {
        let Some(stored) = v2.view(|tx| tx.get_full_tx_path(interval.tx_path_hash))? else {
            report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
            report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
            note(report, "tx_path", format!("offset {}", key.0));
            return Ok(());
        };
        Some(stored)
    } else {
        None
    };
    let derived = match derive_at(db, cache, map, key)? {
        Some(derived) => derived,
        None => {
            report.skipped_frontier = report.skipped_frontier.saturating_add(1);
            if place.block_height.is_some() {
                report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                note(
                    report,
                    "interval",
                    format!(
                        "offset {} is past the consensus frontier but RocksDB has a placement",
                        key.0
                    ),
                );
            }
            return Ok(());
        }
    };
    if let Some(stored) = &stored
        && tx_path_hash(stored) != interval.tx_path_hash
    {
        report.mismatch_tx_path_hash = report.mismatch_tx_path_hash.saturating_add(1);
        note(report, "tx_path_hash", format!("offset {}", key.0));
    }
    let header = db.view_eyre(|tx| tx_header_by_txid(tx, &derived.tx_id))?;
    let Some(header) = header else {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
        note(
            report,
            "tx_path",
            format!("offset {} tx header {} missing", key.0, derived.tx_id),
        );
        return Ok(());
    };
    if let Some(stored) = &stored
        && derived.proof.proof != *stored
    {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
        note(report, "tx_path", format!("offset {}", key.0));
        return Ok(());
    }
    let tx_index = u32::try_from(derived.tx_index).ok();
    let same = place.data_root == derived.data_root
        && place.end == segment_end
        && place.block_height == Some(derived.height)
        && place.tx_index == tx_index
        && place.ledger == Some(map.ledger.get_id());
    if !same {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(report, "interval", format!("offset {}", key.0));
    }
    report.data_roots_checked = report.data_roots_checked.saturating_add(1);
    if is_first && let Some(binding) = &binding {
        report.tx_leaf_bindings_checked = report.tx_leaf_bindings_checked.saturating_add(1);
        if binding.data_root != derived.data_root || binding.prefix_hash != header.prefix_hash {
            report.tx_leaf_bindings_mismatches =
                report.tx_leaf_bindings_mismatches.saturating_add(1);
            note(report, "tx_leaf_binding", format!("offset {}", key.0));
        }
    }
    if binding.is_some() {
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
    } else if is_first && (place.tx_start_offset.0 != 0 || place.data_size != 0) {
        report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
        note(report, "data_root", format!("offset {}", key.0));
    }
    note_chain_extent(
        &place,
        derived.tx_ledger_start,
        header.data_size,
        map,
        key,
        report,
    );
    if is_first {
        note_missing_later_segments(v3, db, cache, map, &segments, key, report)?;
    }
    Ok(())
}

fn note_missing_later_segments(
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    segments: &[(PartitionChunkOffset, PartitionChunkOffset)],
    key: PartitionChunkOffset,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    for (cut, _) in segments.iter().skip_while(|(start, _)| *start <= key) {
        if v3.view(|tx| tx.placement_at(*cut))?.is_some() {
            continue;
        }
        if derive_at(db, cache, map, *cut)?.is_none() {
            report.skipped_frontier = report.skipped_frontier.saturating_add(1);
            break;
        }
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(
            report,
            "interval",
            format!("offset {} has no v3 placement", cut.0),
        );
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
                if left[index].is_some()
                    && right[index].is_none()
                    && v2.view(|tx| tx.tx_interval_at(*key))?.is_none()
                {
                    report.skipped_orphans = report.skipped_orphans.saturating_add(1);
                    continue;
                }
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

fn note_reset(report: &mut EngineEquivalenceReport, differs: bool) {
    if differs {
        report.reset_marker_mismatch = 1;
        note(report, "reset_marker", "metadata".to_owned());
    }
}

fn note_schema(report: &mut EngineEquivalenceReport, ok: bool) {
    if !ok {
        report.schema_mismatch = 1;
        note(report, "schema", "metadata".to_owned());
    }
}

fn verify_markers<S: SubmoduleStore>(
    v2: &S,
    v3: &RocksSubmoduleStore,
    v2_schema: Option<Vec<u8>>,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    note_reset(report, v2.reset_in_progress()? != v3.reset_in_progress()?);
    let schema_ok = v2_schema.as_deref() == Some(SUBMODULE_SCHEMA.as_bytes())
        && v3.schema_bytes()?.as_deref() == Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes());
    note_schema(report, schema_ok);
    Ok(())
}

fn refuse_existing_rocks_schema(path: &Path) -> eyre::Result<()> {
    match rocks_schema_text(path)? {
        Some(text) if text == SUBMODULE_SCHEMA_ROCKS_V1 => eyre::bail!(
            "target {} is a completed RocksDB index. Refusing to replace it.",
            path.display()
        ),
        Some(_) => eyre::bail!(
            "target {} has an existing index schema. Refusing to delete it.",
            path.display()
        ),
        None => Ok(()),
    }
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

fn require_rocks_v1(path: &Path) -> eyre::Result<()> {
    match rocks_schema_text(path)? {
        Some(text) if text == SUBMODULE_SCHEMA_ROCKS_V1 => Ok(()),
        Some(text) => eyre::bail!("{}", super::tables::unrecognised_rocks_layout(&text)),
        None => eyre::bail!(
            "rocks index {} has no RocksDB v1 SCHEMA marker",
            path.display()
        ),
    }
}

fn copy_rows<K, V>(
    mut read: impl FnMut(Option<K>) -> eyre::Result<Vec<(K, V)>>,
    mut write: impl FnMut(Vec<(K, V)>) -> eyre::Result<u64>,
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
        note_rows(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        let n = write(batch)?;
        copied = copied.saturating_add(n);
        if n > 0 {
            progress.note()?;
        }
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
        if pending.len() == merge_batch_limit() {
            note_rows(u64::try_from(pending.len()).unwrap_or(u64::MAX));
            on_batch(&pending)?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        note_rows(u64::try_from(pending.len()).unwrap_or(u64::MAX));
        on_batch(&pending)?;
    }
    Ok(())
}

fn note(report: &mut EngineEquivalenceReport, kind: &'static str, at: String) {
    if report.samples.len() < SAMPLE_LIMIT {
        report.samples.push(EngineMismatch { kind, at });
    }
}

fn guard_paths(src: &Path, dst: &Path, options: &ConvertOptions) -> eyre::Result<()> {
    if same_path(src, dst) {
        eyre::bail!("source and target are the same path ({})", src.display());
    }
    if !src.is_dir() {
        eyre::bail!("source {} is not a directory", src.display());
    }
    if paths_overlap(src, dst) {
        eyre::bail!(
            "source {} and target {} overlap. Refusing to read and write the same tree.",
            src.display(),
            dst.display()
        );
    }
    if let Some(consensus) = &options.consensus_db
        && (same_path(dst, consensus) || paths_overlap(dst, consensus))
    {
        eyre::bail!(
            "target {} overlaps the consensus database {}",
            dst.display(),
            consensus.display()
        );
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    // A `..` that escapes has no safe canonical form. Treat that as overlap.
    let (Ok(left), Ok(right)) = (lexical_target(left), lexical_target(right)) else {
        return true;
    };
    // No existing ancestor, or the current directory could not be read.
    // Refuse the pair.
    let Some(left) = canonical_existing(&left) else {
        return true;
    };
    let Some(right) = canonical_existing(&right) else {
        return true;
    };
    if left == right {
        return false;
    }
    left.starts_with(&right) || right.starts_with(&left)
}

/// A filled placement must match the derived tx start and the header size.
fn note_chain_extent(
    place: &super::tables::Placement,
    tx_ledger_start: u64,
    header_data_size: u64,
    map: &PlacementMap,
    at: PartitionChunkOffset,
    report: &mut EngineEquivalenceReport,
) {
    let filled = place.tx_start_offset.0 != 0 || place.data_size != 0;
    if !filled {
        return;
    }
    let base = ledger_base(map.slot_index, map.chunks_per_partition);
    let expected = i64::try_from(tx_ledger_start)
        .ok()
        .zip(base)
        .and_then(|(start, base)| start.checked_sub(base));
    let start_ok =
        expected.and_then(|value| i32::try_from(value).ok()) == Some(place.tx_start_offset.0);
    if !start_ok || place.data_size != header_data_size {
        report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
        note(report, "data_root", format!("offset {} chain", at.0));
    }
}

/// First ledger offset of the submodule's slot, `slot * chunks_per_partition`.
fn ledger_base(slot_index: u64, chunks_per_partition: u64) -> Option<i64> {
    let slot = i64::try_from(slot_index).ok()?;
    let chunks = i64::try_from(chunks_per_partition).ok()?;
    slot.checked_mul(chunks)
}

/// Canonical path, including components that do not exist yet.
///
/// A relative path is joined to the process current directory before the
/// walk. `current_dir` or `canonicalize` failing returns `None`, and the
/// overlap check refuses that pair.
fn canonical_existing(path: &Path) -> Option<PathBuf> {
    let path = if path.is_relative() {
        std::env::current_dir().ok()?.join(path)
    } else {
        path.to_path_buf()
    };
    let mut current = path;
    let mut suffix = Vec::new();
    loop {
        if current.exists() {
            let mut full = current.canonicalize().ok()?;
            for part in suffix.iter().rev() {
                full.push(part);
            }
            return Some(full);
        }
        suffix.push(current.file_name()?.to_os_string());
        if !current.pop() {
            return None;
        }
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (lexical_target(left), lexical_target(right)) else {
        return false;
    };
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Collapse `.` and `..` without creating missing directories.
///
/// The kernel resolves `..` through a component that does not exist yet and
/// can land in the source index or the storage module. A `..` that climbs
/// past the root, or that leaves a relative path empty, is refused.
fn lexical_target(path: &Path) -> eyre::Result<PathBuf> {
    let mut out = PathBuf::new();
    let unanchored = |collapsed: &Path| {
        let form = if collapsed.as_os_str().is_empty() {
            "\"\"".to_owned()
        } else {
            collapsed.display().to_string()
        };
        eyre::eyre!(
            "target {} is not anchored. The collapsed form is {form}.",
            path.display()
        )
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !pop_normal(&mut out) {
                    return Err(unanchored(&out));
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(unanchored(&out));
    }
    Ok(out)
}

/// Drop one normal component. A root or an empty path stays put.
fn pop_normal(path: &mut PathBuf) -> bool {
    if !matches!(path.components().next_back(), Some(Component::Normal(_))) {
        return false;
    }
    path.pop()
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

fn finish_stats(mut stats: ConvertStats, started: Instant, source_v1: bool) -> ConvertStats {
    stats.source_v1 = source_v1;
    stats.migrated_v1 = false;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    stats
}

/// Offsets whose v1 data path was compared.
///
/// Inserts arrive in ascending order. Adjacent offsets share one inclusive
/// range. A hole is not inserted, so a later check does not treat it as
/// already compared.
#[derive(Default)]
struct AcceptedOffsets {
    ranges: Vec<(u32, u32)>,
}

impl AcceptedOffsets {
    fn insert(&mut self, offset: u32) {
        if let Some(last) = self.ranges.last_mut() {
            if offset <= last.1 {
                return;
            }
            if last.1.checked_add(1) == Some(offset) {
                last.1 = offset;
                return;
            }
        }
        self.ranges.push((offset, offset));
    }

    fn contains(&self, offset: u32) -> bool {
        let index = self.ranges.partition_point(|&(start, _)| start <= offset);
        let Some(&(start, end)) = index.checked_sub(1).and_then(|at| self.ranges.get(at)) else {
            return false;
        };
        start <= offset && offset <= end
    }

    fn range_count(&self) -> usize {
        self.ranges.len()
    }
}

#[derive(Default)]
struct FrontierSkips {
    ranges: std::cell::RefCell<Vec<(u32, u32)>>,
    prepared: std::cell::Cell<bool>,
}

impl FrontierSkips {
    fn covers(&self, offset: u32) -> bool {
        self.prepare();
        let ranges = self.ranges.borrow();
        let index = ranges.partition_point(|&(start, _)| start <= offset);
        let Some(&(start, end)) = index.checked_sub(1).and_then(|at| ranges.get(at)) else {
            return false;
        };
        start <= offset && offset <= end
    }

    fn push(&mut self, start: PartitionChunkOffset, end: PartitionChunkOffset) {
        self.ranges.get_mut().push((start.0, end.0));
        self.prepared.set(false);
    }

    /// Sort and merge so one binary search answers `covers`.
    fn prepare(&self) {
        if self.prepared.get() {
            return;
        }
        let mut ranges = self.ranges.borrow_mut();
        ranges.sort_unstable_by_key(|range| (range.0, range.1));
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for &(start, end) in ranges.iter() {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        *ranges = merged;
        drop(ranges);
        self.prepared.set(true);
    }
}

enum OpenedMdbx {
    V2(MdbxSubmoduleStore),
    V1(DatabaseEnv),
    Empty(DatabaseEnv),
}

fn opened_env(opened: &OpenedMdbx) -> &DatabaseEnv {
    match opened {
        OpenedMdbx::V2(store) => store.env(),
        OpenedMdbx::V1(env) | OpenedMdbx::Empty(env) => env,
    }
}

fn source_row_total(opened: &OpenedMdbx) -> eyre::Result<u64> {
    let v2 = matches!(opened, OpenedMdbx::V2(_));
    opened_env(opened).view_eyre(|tx| {
        let rows = if v2 {
            table_entries::<TxPathIntervalByStart>(tx)?
                .saturating_add(table_entries::<ChunkDataPathByOffset>(tx)?)
                .saturating_add(table_entries::<PendingBodyMigrationsByOffset>(tx)?)
        } else {
            table_entries::<ChunkPathHashesByOffset>(tx)?
                .saturating_add(table_entries::<PendingBodyMigrationsByOffset>(tx)?)
        };
        Ok(u64::try_from(rows).unwrap_or(u64::MAX))
    })
}

// MDBX-SUBMODULE-INDEX: delete this helper with the MDBX source reader.
fn open_mdbx_source(path: &Path, args: DatabaseArguments) -> eyre::Result<OpenedMdbx> {
    if !path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "MDBX index {} has no mdbx.dat. Pass the submodule db/ directory.",
            path.display()
        );
    }
    let env = DatabaseEnv::open(path, DatabaseEnvKind::RO, args.with_exclusive(Some(true)))
        .map_err(|err| eyre::eyre!("read-only open {}: {err}", path.display()))?;
    let bytes = env.view_eyre(read_schema_bytes)?;
    match bytes.as_deref() {
        Some(schema) if schema == SUBMODULE_SCHEMA.as_bytes() => {
            Ok(OpenedMdbx::V2(MdbxSubmoduleStore::from_env(env, false)))
        }
        Some(schema) if schema == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() => {
            eyre::bail!(
                "MDBX index {} is an incomplete conversion. The source was not modified.",
                path.display()
            )
        }
        Some(schema) if schema.starts_with(migrate::PROGRESS_PREFIX.as_bytes()) => {
            eyre::bail!(
                "MDBX index {} has an in-progress v1 migration. The source was not modified. Finish or restore that directory before converting.",
                path.display()
            )
        }
        Some(_) => eyre::bail!(
            "MDBX index {} schema row is not a submodule index. Refusing so the source is not modified.",
            path.display()
        ),
        None if mdbx_has_v1_rows(&env)? => Ok(OpenedMdbx::V1(env)),
        None => Ok(OpenedMdbx::Empty(env)),
    }
}

fn source_reset(opened: &OpenedMdbx) -> eyre::Result<bool> {
    match opened {
        OpenedMdbx::V2(store) => store.reset_in_progress(),
        OpenedMdbx::V1(env) | OpenedMdbx::Empty(env) => reset_marker_on(env),
    }
}

fn reset_marker_on(env: &DatabaseEnv) -> eyre::Result<bool> {
    env.view_eyre(
        |tx| match tx.get::<Metadata>(MetadataKey::SubmoduleResetInProgress) {
            Ok(value) => Ok(value.is_some()),
            Err(DatabaseError::Decode) => Ok(true),
            Err(err) if table_missing(&err) => Ok(false),
            Err(err) => Err(eyre::eyre!("read reset marker: {err}")),
        },
    )
}

fn mdbx_has_v1_rows(env: &DatabaseEnv) -> eyre::Result<bool> {
    env.view_eyre(|tx| {
        let hashes = table_entries::<ChunkPathHashesByOffset>(tx)?;
        if hashes > 0 {
            return Ok(true);
        }
        let paths = table_entries::<ChunkDataPathByPathHash>(tx)?;
        Ok(paths > 0)
    })
}

fn table_entries<T>(tx: &impl reth_db::transaction::DbTx) -> eyre::Result<usize>
where
    T: reth_db::table::Table,
{
    match tx.entries::<T>() {
        Ok(count) => Ok(count),
        Err(err) if table_missing(&err) => Ok(0),
        Err(err) => Err(err.into()),
    }
}

fn table_missing(err: &DatabaseError) -> bool {
    migrate::missing_table(err)
}

fn write_rocks_copy(
    highest: eyre::Result<Option<PartitionChunkOffset>>,
    dir: &Path,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
    copy: impl FnOnce(Option<&RocksSubmoduleStore>) -> eyre::Result<ConvertStats>,
) -> eyre::Result<ConvertStats> {
    ensure_consensus_covers(db, map, highest?, options.allow_unresolved)?;
    if options.dry_run {
        let stats = copy(None)?;
        refuse_unresolved(&stats, options)?;
        return Ok(stats);
    }
    let lock = prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting_with_lock(
        dir,
        Some(lock),
    ))?;
    let stats = copy(Some(&dst))?;
    refuse_unresolved(&stats, options)?;
    seal_rocks_target(dst, options)?;
    clear_converting_sentinel(dir)?;
    Ok(stats)
}

fn copy_v2_rocks<S>(
    src: &S,
    dir: &Path,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats>
where
    S: RowSource + SubmoduleStore,
{
    write_rocks_copy(src.highest_placement_end(), dir, options, db, map, |dst| {
        copy_mdbx_into_rocks(src, dst, options, db, map)
    })
}

fn open_converting_rocks(
    dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<RocksSubmoduleStore> {
    let lock = prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting_with_lock(
        dir,
        Some(lock),
    ))?;
    if options.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    Ok(dst)
}

fn write_empty_rocks(dir: &Path, options: &ConvertOptions) -> eyre::Result<ConvertStats> {
    if options.dry_run {
        return Ok(ConvertStats::default());
    }
    let dst = open_converting_rocks(dir, options)?;
    seal_rocks_target(dst, options)?;
    clear_converting_sentinel(dir)?;
    Ok(ConvertStats::default())
}

fn write_reset_rocks(
    dir: &Path,
    options: &ConvertOptions,
    source_v1: bool,
) -> eyre::Result<ConvertStats> {
    if options.dry_run {
        return Ok(ConvertStats {
            reset_marker: true,
            source_v1,
            ..ConvertStats::default()
        });
    }
    let dst = open_converting_rocks(dir, options)?;
    dst.set_reset_in_progress()?;
    seal_rocks_target(dst, options)?;
    clear_converting_sentinel(dir)?;
    Ok(ConvertStats {
        reset_marker: true,
        source_v1,
        ..ConvertStats::default()
    })
}

fn write_reset_mdbx(dir: &Path, options: &ConvertOptions) -> eyre::Result<ConvertStats> {
    if options.dry_run {
        return Ok(ConvertStats {
            reset_marker: true,
            ..ConvertStats::default()
        });
    }
    let args = exclusive_args(options);
    // Held until the completion marker is durable. Drop releases `LOCK`.
    let _lock = prepare_mdbx_target(dir, args.clone(), options.replace_incomplete)?;
    let dst = map_open(MdbxSubmoduleStore::open_for_convert(dir, args))?;
    if options.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    dst.set_reset_in_progress()?;
    dst.put_schema_bytes(SUBMODULE_SCHEMA.as_bytes())?;
    clear_converting_sentinel(dir)?;
    Ok(ConvertStats {
        reset_marker: true,
        ..ConvertStats::default()
    })
}

fn copy_v1_rocks(
    env: &DatabaseEnv,
    dir: &Path,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats> {
    write_rocks_copy(highest_v1_placement(env), dir, options, db, map, |dst| {
        copy_mdbx_v1_into_rocks(env, dst, options, db, map)
    })
}

/// Last offset that carries a tx-path hash. An orphan data-path row past that
/// offset is not a placement.
fn highest_v1_placement(env: &DatabaseEnv) -> eyre::Result<Option<PartitionChunkOffset>> {
    env.view_eyre(|tx| {
        let mut cursor = match tx.cursor_read::<ChunkPathHashesByOffset>() {
            Ok(cursor) => cursor,
            Err(err) if table_missing(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let mut current = cursor.last()?;
        while let Some((offset, hashes)) = current {
            if hashes.tx_path_hash.is_some() {
                return Ok(Some(offset));
            }
            current = cursor.prev()?;
        }
        Ok(None)
    })
}

/// Refuse a consensus database that does not cover `highest`.
///
/// An empty block index is always an error, including when unresolved
/// placements are allowed. A source with no placement skips this check.
fn ensure_consensus_covers(
    db: &DatabaseEnv,
    map: &PlacementMap,
    highest: Option<PartitionChunkOffset>,
    allow_unresolved: bool,
) -> eyre::Result<()> {
    let Some(highest) = highest else {
        return Ok(());
    };
    let ledger_offset = map.ledger_offset(highest)?;
    let frontier = ledger_frontier(db, map.ledger).map_err(|err| {
        eyre::eyre!(
            "{err:#}. An empty or unreadable consensus block index is refused even with --allow-unresolved."
        )
    })?;
    if ledger_offset < frontier {
        return Ok(());
    }
    let summary = format!(
        "consensus database does not cover the highest placement: partition offset {} is ledger {} offset {ledger_offset}, frontier is {frontier}",
        highest.0,
        map.ledger.get_id(),
    );
    if allow_unresolved {
        tracing::warn!("{summary}");
        return Ok(());
    }
    eyre::bail!(
        "{summary}. The target was not written. Point --consensus-db at a database that includes this height, or pass --allow-unresolved to write a partial index."
    )
}

/// A convert that skipped placements is not a successful index.
fn refuse_unresolved(stats: &ConvertStats, options: &ConvertOptions) -> eyre::Result<()> {
    if stats.skipped_frontier == 0 || options.allow_unresolved {
        return Ok(());
    }
    let outcome = if options.dry_run {
        "The target was not created."
    } else {
        "The target has no completion marker."
    };
    eyre::bail!(
        "consensus database left {} placement(s) unresolved (skipped_frontier={}). {outcome} Point --consensus-db at a database that contains those blocks, or pass --allow-unresolved.",
        stats.skipped_frontier,
        stats.skipped_frontier,
    )
}

struct V1Ready {
    placement: PreparedPlacement,
    paths: Vec<(PartitionChunkOffset, ChunkDataPath)>,
    path_bytes: usize,
}

struct V1Out {
    items: Vec<V1Ready>,
    placements: usize,
    bytes: usize,
}

struct V1Src<'a> {
    env: &'a DatabaseEnv,
    db: &'a DatabaseEnv,
    cache: &'a mut TxPathBlockCache,
    map: &'a PlacementMap,
}

struct V1Sink<'a> {
    dst: Option<&'a RocksSubmoduleStore>,
    limits: Limits,
    progress: &'a mut CopyProgress,
    stats: &'a mut ConvertStats,
    written: &'a mut FrontierSkips,
    frontier: &'a mut FrontierSkips,
    out: &'a mut V1Out,
}

/// One streaming read of the v1 hash table. Placements and their data paths
/// share a durable commit, up to `batch_rows` placements or `batch_value_bytes`.
fn copy_mdbx_v1_into_rocks(
    env: &DatabaseEnv,
    dst: Option<&RocksSubmoduleStore>,
    options: &ConvertOptions,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<ConvertStats> {
    let mut progress = CopyProgress::from_options(options);
    if !options.dry_run && progress.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
    let limits = Limits::from_options(options);
    let mut cache = TxPathBlockCache::new();
    let mut written = FrontierSkips::default();
    let mut frontier = FrontierSkips::default();
    let mut stats = ConvertStats::default();
    let mut out = V1Out {
        items: Vec::new(),
        placements: 0,
        bytes: 0,
    };
    let mut open = None;
    let mut paths = Vec::new();
    let mut path_bytes = 0_usize;
    let mut rescan = false;
    let mut after = None;
    let mut orphans = 0_u64;
    {
        let mut src = V1Src {
            env,
            db,
            cache: &mut cache,
            map,
        };
        let mut sink = V1Sink {
            dst,
            limits,
            progress: &mut progress,
            stats: &mut stats,
            written: &mut written,
            frontier: &mut frontier,
            out: &mut out,
        };
        loop {
            let batch = read_v1_hash_batch(env, after, limits)?;
            if batch.is_empty() {
                if let Some(run) = open.take() {
                    close_v1_run(&mut src, &mut sink, run, std::mem::take(&mut paths), rescan)?;
                }
                break;
            }
            note_rows(u64::try_from(batch.len()).unwrap_or(u64::MAX));
            for (offset, hashes) in &batch {
                let mut closed = Vec::new();
                migrate::absorb(&mut open, offset.0, hashes.tx_path_hash, &mut closed);
                if let Some(run) = closed.pop() {
                    let taken_rescan = rescan;
                    path_bytes = 0;
                    rescan = false;
                    close_v1_run(
                        &mut src,
                        &mut sink,
                        run,
                        std::mem::take(&mut paths),
                        taken_rescan,
                    )?;
                }
                if hashes.tx_path_hash.is_none() {
                    if let Some(hash) = hashes.data_path_hash {
                        let _path = read_v1_data_path(env, *offset, hash)?;
                        orphans = orphans.saturating_add(1);
                    }
                } else if let Some(hash) = hashes.data_path_hash {
                    // Once `rescan` is set, `copy_v1_range_paths` reads this run.
                    if rescan {
                        continue;
                    }
                    let path = read_v1_data_path(env, *offset, hash)?;
                    attach_v1_path(
                        limits,
                        &mut paths,
                        &mut path_bytes,
                        &mut rescan,
                        *offset,
                        path,
                    );
                }
            }
            after = batch.last().map(|(key, _)| *key);
        }
        flush_v1_out(&mut sink)?;
    }
    stats.skipped_orphans = stats.skipped_orphans.saturating_add(orphans);
    let (pending, orphan_pending, frontier_pending) =
        copy_v1_pending(env, dst, &written, &frontier, limits, &mut progress)?;
    stats.pending = pending;
    stats.skipped_orphans = stats.skipped_orphans.saturating_add(orphan_pending);
    stats.skipped_frontier = stats.skipped_frontier.saturating_add(frontier_pending);
    Ok(stats)
}

fn attach_v1_path(
    limits: Limits,
    paths: &mut Vec<(PartitionChunkOffset, ChunkDataPath)>,
    path_bytes: &mut usize,
    rescan: &mut bool,
    offset: PartitionChunkOffset,
    path: ChunkDataPath,
) {
    if *rescan {
        return;
    }
    let len = path.len();
    if !paths.is_empty() && path_bytes.saturating_add(len) > limits.value_bytes {
        *rescan = true;
        paths.clear();
        *path_bytes = 0;
        return;
    }
    *path_bytes = path_bytes.saturating_add(len);
    paths.push((offset, path));
}

fn close_v1_run(
    src: &mut V1Src<'_>,
    sink: &mut V1Sink<'_>,
    run: OpenRun,
    mut paths: Vec<(PartitionChunkOffset, ChunkDataPath)>,
    rescan: bool,
) -> eyre::Result<()> {
    let prepared = prepare_v1_placement(src.env, src.db, src.cache, src.map, &run)?;
    for (start, end) in prepared.frontier {
        sink.stats.skipped_frontier = sink.stats.skipped_frontier.saturating_add(1);
        sink.frontier.push(start, end);
    }
    if prepared.placements.is_empty() {
        return Ok(());
    }
    let written_start = prepared.placements[0].start;
    let written_end = prepared
        .placements
        .last()
        .map(|item| item.end)
        .unwrap_or(written_start);
    if rescan {
        for placement in prepared.placements {
            push_v1_ready(
                sink,
                V1Ready {
                    placement,
                    paths: Vec::new(),
                    path_bytes: 0,
                },
            )?;
        }
        flush_v1_out(sink)?;
        let copied = copy_v1_range_paths(
            src.env,
            sink.dst,
            written_start,
            written_end,
            sink.limits,
            sink.progress,
        )?;
        sink.stats.data_paths = sink.stats.data_paths.saturating_add(copied);
        sink.stats.path_hashes = sink.stats.data_paths;
        return Ok(());
    }
    for placement in prepared.placements {
        let end = placement.end;
        let seg_paths = take_paths_through(&mut paths, end);
        let seg_bytes = seg_paths.iter().map(|(_, path)| path.len()).sum();
        push_v1_ready(
            sink,
            V1Ready {
                placement,
                paths: seg_paths,
                path_bytes: seg_bytes,
            },
        )?;
    }
    Ok(())
}

fn take_paths_through(
    paths: &mut Vec<(PartitionChunkOffset, ChunkDataPath)>,
    end: PartitionChunkOffset,
) -> Vec<(PartitionChunkOffset, ChunkDataPath)> {
    let count = paths
        .iter()
        .take_while(|(offset, _)| *offset <= end)
        .count();
    paths.drain(..count).collect()
}

fn push_v1_ready(sink: &mut V1Sink<'_>, ready: V1Ready) -> eyre::Result<()> {
    let add_bytes = ready.path_bytes;
    let full = !sink.out.items.is_empty()
        && (sink.out.placements >= sink.limits.rows
            || sink.out.bytes.saturating_add(add_bytes) > sink.limits.value_bytes);
    if full {
        flush_v1_out(sink)?;
    }
    sink.stats.intervals = sink.stats.intervals.saturating_add(1);
    sink.stats.tx_paths = sink.stats.intervals;
    sink.stats.tx_leaf_bindings = sink.stats.intervals;
    sink.stats.data_paths = sink
        .stats
        .data_paths
        .saturating_add(u64::try_from(ready.paths.len()).unwrap_or(u64::MAX));
    sink.stats.path_hashes = sink.stats.data_paths;
    if ready.placement.info.is_some() {
        sink.stats.data_roots = sink.stats.data_roots.saturating_add(1);
    }
    sink.written
        .push(ready.placement.start, ready.placement.end);
    sink.out.placements = sink.out.placements.saturating_add(1);
    sink.out.bytes = sink.out.bytes.saturating_add(add_bytes);
    sink.out.items.push(ready);
    Ok(())
}

fn flush_v1_out(sink: &mut V1Sink<'_>) -> eyre::Result<()> {
    if sink.out.items.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(&mut sink.out.items);
    sink.out.placements = 0;
    sink.out.bytes = 0;
    if let Some(dst) = sink.dst {
        dst.update(|tx| {
            for item in items {
                write_rocks_placement(tx, &item.placement)?;
                for (offset, path) in item.paths {
                    require_interval(tx, offset, "data path")?;
                    tx.add_full_data_path(offset, path)?;
                }
            }
            Ok(())
        })?;
        sink.progress.note()?;
    }
    Ok(())
}

fn prepare_v1_placement(
    env: &DatabaseEnv,
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    run: &OpenRun,
) -> eyre::Result<PreparedRun> {
    let start = PartitionChunkOffset(run.start);
    let end = PartitionChunkOffset(run.end);
    let stored = read_v1_tx_path(env, run.tx_hash)?
        .ok_or_else(|| eyre::eyre!("partition offset {} has no stored tx path", start.0))?;
    if tx_path_hash(&stored) != run.tx_hash {
        eyre::bail!(
            "tx path hash at partition offset {} does not match the stored path",
            start.0
        );
    }
    let binding = read_v1_binding(env, run.tx_hash)?;
    split_stored_interval(
        db,
        cache,
        map,
        SplitInput {
            start,
            end,
            stored: &stored,
            path_hash: run.tx_hash,
            binding,
        },
        |root| v1_infos(env, root),
    )
}

fn walk_v1_intervals(
    env: &DatabaseEnv,
    limits: Limits,
    mut on_interval: impl FnMut(OpenRun) -> eyre::Result<()>,
) -> eyre::Result<u64> {
    let mut open = None;
    let mut after = None;
    let mut orphans = 0_u64;
    loop {
        let batch = read_v1_hash_batch(env, after, limits)?;
        if batch.is_empty() {
            if let Some(run) = open.take() {
                on_interval(run)?;
            }
            break;
        }
        note_rows(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        for (offset, hashes) in &batch {
            let mut closed = Vec::new();
            migrate::absorb(&mut open, offset.0, hashes.tx_path_hash, &mut closed);
            for run in closed {
                on_interval(run)?;
            }
            if hashes.tx_path_hash.is_none()
                && let Some(hash) = hashes.data_path_hash
            {
                let _path = read_v1_data_path(env, *offset, hash)?;
                orphans = orphans.saturating_add(1);
            }
        }
        after = batch.last().map(|(key, _)| *key);
    }
    Ok(orphans)
}

fn read_v1_hash_batch(
    env: &DatabaseEnv,
    after: Option<PartitionChunkOffset>,
    limits: Limits,
) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
    let rows = limits.rows.max(1);
    let value_bytes = limits.value_bytes.max(1);
    env.view_eyre(|tx| {
        let mut cursor = match tx.cursor_read::<ChunkPathHashesByOffset>() {
            Ok(cursor) => cursor,
            Err(err) if table_missing(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let walker = cursor.walk(after)?;
        let mut out = Vec::new();
        let mut bytes = 0_usize;
        for entry in walker {
            let (key, value) = entry?;
            if after.is_some_and(|prev| key <= prev) {
                continue;
            }
            let next_bytes = bytes.saturating_add(32);
            if !out.is_empty() && (out.len() >= rows || next_bytes > value_bytes) {
                break;
            }
            bytes = next_bytes;
            out.push((key, value));
            if out.len() >= rows || bytes >= value_bytes {
                break;
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
thread_local! {
    static V1_DATA_PATH_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    static VERIFY_VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_verify_views() {
    VERIFY_VIEWS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn verify_views() -> usize {
    VERIFY_VIEWS.with(std::cell::Cell::get)
}

fn note_verify_view() {
    #[cfg(test)]
    VERIFY_VIEWS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
fn reset_v1_data_path_reads() {
    V1_DATA_PATH_READS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn v1_data_path_reads() -> usize {
    V1_DATA_PATH_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    static V1_COPY_PATH_VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_v1_copy_path_views() {
    V1_COPY_PATH_VIEWS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn v1_copy_path_views() -> usize {
    V1_COPY_PATH_VIEWS.with(std::cell::Cell::get)
}

fn note_v1_copy_path_view() {
    #[cfg(test)]
    V1_COPY_PATH_VIEWS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
thread_local! {
    static V1_UNCOMPARED_HASH_VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static V1_UNCOMPARED_HASH_GROUP: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn reset_v1_uncompared_hash_views() {
    V1_UNCOMPARED_HASH_VIEWS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn v1_uncompared_hash_views() -> usize {
    V1_UNCOMPARED_HASH_VIEWS.with(std::cell::Cell::get)
}

fn note_v1_uncompared_hash_view() {
    #[cfg(test)]
    V1_UNCOMPARED_HASH_VIEWS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
struct V1UncomparedHashGroupGuard;

#[cfg(test)]
impl Drop for V1UncomparedHashGroupGuard {
    fn drop(&mut self) {
        V1_UNCOMPARED_HASH_GROUP.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
fn set_v1_uncompared_hash_group(group: usize) -> V1UncomparedHashGroupGuard {
    V1_UNCOMPARED_HASH_GROUP.with(|cell| cell.set(Some(group.max(1))));
    V1UncomparedHashGroupGuard
}

fn uncompared_hash_group() -> usize {
    #[cfg(test)]
    if let Some(group) = V1_UNCOMPARED_HASH_GROUP.with(std::cell::Cell::get) {
        return group.max(1);
    }
    usize::MAX
}

#[cfg(test)]
thread_local! {
    static V1_PATH_VIEW_GROUP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct V1PathViewGroupGuard;

#[cfg(test)]
impl Drop for V1PathViewGroupGuard {
    fn drop(&mut self) {
        V1_PATH_VIEW_GROUP.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
fn set_v1_path_view_group(group: usize) -> V1PathViewGroupGuard {
    V1_PATH_VIEW_GROUP.with(|cell| cell.set(Some(group.max(1))));
    V1PathViewGroupGuard
}

fn v1_path_view_group() -> usize {
    #[cfg(test)]
    if let Some(group) = V1_PATH_VIEW_GROUP.with(std::cell::Cell::get) {
        return group.max(1);
    }
    usize::MAX
}

#[cfg(test)]
thread_local! {
    static ACCEPTED_OFFSET_RANGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_accepted_offset_ranges() {
    ACCEPTED_OFFSET_RANGES.with(|cell| cell.set(0));
}

#[cfg(test)]
fn accepted_offset_ranges() -> usize {
    ACCEPTED_OFFSET_RANGES.with(std::cell::Cell::get)
}

#[cfg_attr(not(test), expect(unused_variables))]
fn note_accepted_offset_ranges(count: usize) {
    #[cfg(test)]
    ACCEPTED_OFFSET_RANGES.with(|cell| cell.set(count));
}

#[cfg(test)]
thread_local! {
    static V1_PLACEMENT_VIEWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static V1_PLACEMENT_VIEW_COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static V1_PLACEMENT_BATCH_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
struct CountV1PlacementViews;

#[cfg(test)]
impl CountV1PlacementViews {
    fn enter() -> Self {
        V1_PLACEMENT_VIEW_COUNTING.with(|cell| cell.set(true));
        V1_PLACEMENT_VIEWS.with(|cell| cell.set(0));
        Self
    }
}

#[cfg(test)]
impl Drop for CountV1PlacementViews {
    fn drop(&mut self) {
        V1_PLACEMENT_VIEW_COUNTING.with(|cell| cell.set(false));
    }
}

#[cfg(test)]
fn v1_placement_views() -> usize {
    V1_PLACEMENT_VIEWS.with(std::cell::Cell::get)
}

fn note_v1_placement_view() {
    #[cfg(test)]
    if V1_PLACEMENT_VIEW_COUNTING.with(std::cell::Cell::get) {
        V1_PLACEMENT_VIEWS.with(|cell| cell.set(cell.get().saturating_add(1)));
    }
}

#[cfg(test)]
struct V1PlacementBatchGuard;

#[cfg(test)]
impl Drop for V1PlacementBatchGuard {
    fn drop(&mut self) {
        V1_PLACEMENT_BATCH_OVERRIDE.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
fn set_v1_placement_batch(batch: usize) -> V1PlacementBatchGuard {
    V1_PLACEMENT_BATCH_OVERRIDE.with(|cell| cell.set(Some(batch.max(1))));
    V1PlacementBatchGuard
}

fn placement_verify_batch() -> usize {
    #[cfg(test)]
    if let Some(batch) = V1_PLACEMENT_BATCH_OVERRIDE.with(std::cell::Cell::get) {
        return batch.max(1);
    }
    VERIFY_KEYS
}

#[cfg(test)]
thread_local! {
    static VERIFY_PLACEMENT_CACHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MERGE_BATCH_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn reset_verify_placement_caches() {
    VERIFY_PLACEMENT_CACHES.with(|cell| cell.set(0));
}

#[cfg(test)]
fn verify_placement_caches() -> usize {
    VERIFY_PLACEMENT_CACHES.with(std::cell::Cell::get)
}

fn note_verify_placement_cache() {
    #[cfg(test)]
    VERIFY_PLACEMENT_CACHES.with(|cell| cell.set(cell.get().saturating_add(1)));
}

#[cfg(test)]
struct MergeBatchGuard;

#[cfg(test)]
impl Drop for MergeBatchGuard {
    fn drop(&mut self) {
        MERGE_BATCH_OVERRIDE.with(|cell| cell.set(None));
    }
}

#[cfg(test)]
fn set_merge_batch(batch: usize) -> MergeBatchGuard {
    MERGE_BATCH_OVERRIDE.with(|cell| cell.set(Some(batch.max(1))));
    MergeBatchGuard
}

fn merge_batch_limit() -> usize {
    #[cfg(test)]
    if let Some(batch) = MERGE_BATCH_OVERRIDE.with(std::cell::Cell::get) {
        return batch.max(1);
    }
    VERIFY_KEYS
}

#[cfg(test)]
thread_local! {
    static V1_RANGE_COPY_BATCHES: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn reset_v1_range_copy_batches() {
    V1_RANGE_COPY_BATCHES.with(|cell| cell.borrow_mut().clear());
}

#[cfg(test)]
fn v1_range_copy_batches() -> Vec<usize> {
    V1_RANGE_COPY_BATCHES.with(|cell| cell.borrow().clone())
}

#[cfg_attr(not(test), expect(unused_variables))]
fn note_v1_range_copy_batch(bytes: usize) {
    #[cfg(test)]
    V1_RANGE_COPY_BATCHES.with(|cell| cell.borrow_mut().push(bytes));
}

fn read_v1_data_path(
    env: &DatabaseEnv,
    offset: PartitionChunkOffset,
    hash: H256,
) -> eyre::Result<ChunkDataPath> {
    #[cfg(test)]
    V1_DATA_PATH_READS.with(|cell| cell.set(cell.get().saturating_add(1)));
    env.view_eyre(|tx| v1_data_path_in_tx(tx, offset, hash))
}

fn read_v1_data_paths(
    env: &DatabaseEnv,
    rows: &[(PartitionChunkOffset, H256)],
    count_view: bool,
) -> eyre::Result<Vec<ChunkDataPath>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(rows.len());
    for chunk in rows.chunks(v1_path_view_group()) {
        let paths = env.view_eyre(|tx| {
            // The copy rescan shares this reader and is not a verify view.
            if count_view {
                note_verify_view();
            } else {
                note_v1_copy_path_view();
            }
            chunk
                .iter()
                .map(|(offset, hash)| v1_data_path_in_tx(tx, *offset, *hash))
                .collect::<eyre::Result<Vec<_>>>()
        })?;
        out.extend(paths);
    }
    Ok(out)
}

fn v1_data_path_in_tx(
    tx: &impl reth_db::transaction::DbTx,
    offset: PartitionChunkOffset,
    hash: H256,
) -> eyre::Result<ChunkDataPath> {
    match tx.get::<ChunkDataPathByPathHash>(hash) {
        Ok(Some(path)) => Ok(path),
        Ok(None) => eyre::bail!(
            "submodule index v1 data path for offset {} hash {hash:x} is missing",
            offset.0
        ),
        Err(err) if table_missing(&err) => eyre::bail!(
            "submodule index v1 data path table is missing for offset {} hash {hash:x}",
            offset.0
        ),
        Err(err) => Err(err.into()),
    }
}

fn read_v1_row<T>(env: &DatabaseEnv, key: T::Key) -> eyre::Result<Option<T::Value>>
where
    T: reth_db::table::Table,
    T::Key: Copy,
{
    note_v1_placement_view();
    env.view_eyre(|tx| match tx.get::<T>(key) {
        Ok(row) => Ok(row),
        Err(err) if table_missing(&err) => Ok(None),
        Err(err) => Err(err.into()),
    })
}

fn read_v1_tx_path(env: &DatabaseEnv, hash: H256) -> eyre::Result<Option<TxPath>> {
    read_v1_row::<TxPathByTxPathHash>(env, hash)
}

fn read_v1_binding(env: &DatabaseEnv, hash: H256) -> eyre::Result<Option<TxLeafBinding>> {
    read_v1_row::<TxLeafBindingByTxPathHash>(env, hash)
}

fn v1_infos(env: &DatabaseEnv, data_root: H256) -> eyre::Result<Vec<DataRootInfo>> {
    Ok(read_v1_row::<DataRootInfosByDataRoot>(env, data_root)?
        .map(|DataRootInfos(infos)| infos)
        .unwrap_or_default())
}

fn walk_v1_path_range(
    env: &DatabaseEnv,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    limits: Limits,
    mut on_rows: impl FnMut(Vec<(PartitionChunkOffset, H256)>) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let mut after = start.0.checked_sub(1).map(PartitionChunkOffset);
    loop {
        let batch = read_v1_hash_batch(env, after, limits)?;
        if batch.is_empty() {
            break;
        }
        let mut passed = false;
        let mut rows = Vec::new();
        for (offset, hashes) in &batch {
            if *offset > end {
                passed = true;
                break;
            }
            if let Some(hash) = hashes.data_path_hash {
                rows.push((*offset, hash));
            }
        }
        if !rows.is_empty() {
            on_rows(rows)?;
        }
        let last = batch.last().map(|(key, _)| *key);
        if passed || last.is_some_and(|key| key >= end) {
            break;
        }
        after = last;
    }
    Ok(())
}

fn copy_v1_range_paths(
    env: &DatabaseEnv,
    dst: Option<&RocksSubmoduleStore>,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    limits: Limits,
    progress: &mut CopyProgress,
) -> eyre::Result<u64> {
    let mut copied = 0_u64;
    walk_v1_path_range(env, start, end, limits, |rows| {
        // One view for the hash page. The byte cap still splits the writes.
        let paths = read_v1_data_paths(env, &rows, false)?;
        let mut kept = Vec::new();
        let mut path_bytes = 0_usize;
        for ((offset, _), path) in rows.into_iter().zip(paths) {
            let len = path.len();
            // Same cap as `attach_v1_path`. An empty batch still takes the
            // next path, including one path that is already over the cap.
            if !kept.is_empty() && path_bytes.saturating_add(len) > limits.value_bytes {
                copied = copied.saturating_add(commit_v1_range_paths(dst, progress, &mut kept)?);
                path_bytes = 0;
            }
            path_bytes = path_bytes.saturating_add(len);
            kept.push((offset, path));
        }
        if !kept.is_empty() {
            copied = copied.saturating_add(commit_v1_range_paths(dst, progress, &mut kept)?);
        }
        Ok(())
    })?;
    Ok(copied)
}

fn commit_v1_range_paths(
    dst: Option<&RocksSubmoduleStore>,
    progress: &mut CopyProgress,
    kept: &mut Vec<(PartitionChunkOffset, ChunkDataPath)>,
) -> eyre::Result<u64> {
    let n = u64::try_from(kept.len()).unwrap_or(u64::MAX);
    let batch_bytes = kept.iter().map(|(_, path)| path.len()).sum();
    note_v1_range_copy_batch(batch_bytes);
    if let Some(dst) = dst {
        let batch = std::mem::take(kept);
        dst.update(|tx| {
            for (offset, path) in batch {
                require_interval(tx, offset, "data path")?;
                tx.add_full_data_path(offset, path)?;
            }
            Ok(())
        })?;
        progress.note()?;
    } else {
        kept.clear();
    }
    Ok(n)
}

fn copy_v1_pending(
    env: &DatabaseEnv,
    dst: Option<&RocksSubmoduleStore>,
    written: &FrontierSkips,
    frontier: &FrontierSkips,
    limits: Limits,
    progress: &mut CopyProgress,
) -> eyre::Result<(u64, u64, u64)> {
    let mut after = None;
    let mut copied = 0_u64;
    let mut orphans = 0_u64;
    let mut skipped = 0_u64;
    loop {
        let batch = read_v1_pending_batch(env, after, limits.rows)?;
        if batch.is_empty() {
            break;
        }
        note_rows(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        let mut kept = Vec::new();
        for (offset, job) in batch {
            if written.covers(offset.0) {
                kept.push((offset, job));
            } else if frontier.covers(offset.0) {
                skipped = skipped.saturating_add(1);
            } else {
                orphans = orphans.saturating_add(1);
            }
            after = Some(offset);
        }
        if kept.is_empty() {
            continue;
        }
        let n = u64::try_from(kept.len()).unwrap_or(u64::MAX);
        if let Some(dst) = dst {
            dst.update(|tx| {
                for (offset, job) in &kept {
                    require_interval(tx, *offset, "pending body migration")?;
                    tx.add_pending_body_migration(*offset, job)?;
                }
                Ok(())
            })?;
            progress.note()?;
        }
        copied = copied.saturating_add(n);
    }
    Ok((copied, orphans, skipped))
}

fn read_v1_pending_batch(
    env: &DatabaseEnv,
    after: Option<PartitionChunkOffset>,
    limit: usize,
) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
    let limit = limit.max(1);
    env.view_eyre(|tx| {
        let mut cursor = match tx.cursor_read::<PendingBodyMigrationsByOffset>() {
            Ok(cursor) => cursor,
            Err(err) if table_missing(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let walker = cursor.walk(after)?;
        let mut out = Vec::new();
        for entry in walker {
            let (key, value) = entry?;
            if after.is_some_and(|prev| key <= prev) {
                continue;
            }
            out.push((key, value));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    })
}

fn cached_tx_path_hash(
    tx: &impl reth_db::transaction::DbTx,
    offset: PartitionChunkOffset,
    cached: &mut Option<(PartitionChunkOffset, Option<H256>)>,
) -> eyre::Result<Option<H256>> {
    if let Some((at, hash)) = *cached
        && at == offset
    {
        return Ok(hash);
    }
    let hash = match tx.get::<ChunkPathHashesByOffset>(offset) {
        Ok(row) => row.and_then(|row| row.tx_path_hash),
        Err(err) if table_missing(&err) => None,
        Err(err) => return Err(err.into()),
    };
    *cached = Some((offset, hash));
    Ok(hash)
}

/// A rocks placement start is a v1 run start, or an info cut inside that same run.
///
/// The last hash kept is the one at `start`. The next placement's previous
/// offset is often that start, so the following check does not read it again.
fn placement_start_ok_in_tx(
    tx: &impl reth_db::transaction::DbTx,
    start: PartitionChunkOffset,
    data_root: H256,
    cached: &mut Option<(PartitionChunkOffset, Option<H256>)>,
) -> eyre::Result<bool> {
    let hash = cached_tx_path_hash(tx, start, cached)?;
    let prev = if start.0 == 0 {
        None
    } else {
        cached_tx_path_hash(tx, PartitionChunkOffset(start.0 - 1), cached)?
    };
    *cached = Some((start, hash));
    let starts = matches!(hash, Some(hash) if start.0 == 0 || prev != Some(hash));
    if starts {
        return Ok(true);
    }
    if start.0 == 0 || hash.is_none() || prev != hash {
        return Ok(false);
    }
    let Ok(at) = i32::try_from(start.0) else {
        return Ok(false);
    };
    let infos = match tx.get::<DataRootInfosByDataRoot>(data_root) {
        Ok(Some(DataRootInfos(infos))) => infos,
        Ok(None) => return Ok(false),
        Err(err) if table_missing(&err) => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(infos.iter().any(|info| info.start_offset.0 == at))
}

fn v1_placement_starts_ok(
    env: &DatabaseEnv,
    batch: &[super::tables::Placement],
    cached: &mut Option<(PartitionChunkOffset, Option<H256>)>,
) -> eyre::Result<Vec<bool>> {
    note_v1_placement_view();
    env.view_eyre(|tx| {
        let mut out = Vec::with_capacity(batch.len());
        for place in batch {
            out.push(placement_start_ok_in_tx(
                tx,
                place.start,
                place.data_root,
                cached,
            )?);
        }
        Ok(out)
    })
}

/// Hash rows for covered offsets that `compare_v1_paths` did not already read.
/// One view for the batch. This is not a point read.
fn v1_uncompared_hash_rows(
    env: &DatabaseEnv,
    keys: &[PartitionChunkOffset],
) -> eyre::Result<Vec<(PartitionChunkOffset, Option<ChunkPathHashes>)>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(uncompared_hash_group()) {
        let rows = env.view_eyre(|tx| {
            note_verify_view();
            note_v1_uncompared_hash_view();
            chunk
                .iter()
                .map(|key| match tx.get::<ChunkPathHashesByOffset>(*key) {
                    Ok(row) => Ok(row),
                    Err(err) if table_missing(&err) => Ok(None),
                    Err(err) => Err(err.into()),
                })
                .collect::<eyre::Result<Vec<_>>>()
        })?;
        out.extend(chunk.iter().copied().zip(rows));
    }
    Ok(out)
}

fn verify_mdbx_v1_against_rocks(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<EngineEquivalenceReport> {
    let mut report = EngineEquivalenceReport::default();
    note_schema(
        &mut report,
        v3.schema_bytes()?.as_deref() == Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()),
    );
    note_reset(
        &mut report,
        reset_marker_on(env)? != v3.reset_in_progress()?,
    );
    let limits = Limits {
        rows: VERIFY_KEYS,
        value_bytes: usize::MAX,
    };
    let mut cache = TxPathBlockCache::new();
    let mut written = FrontierSkips::default();
    let mut frontier = FrontierSkips::default();
    let mut accepted = AcceptedOffsets::default();
    let orphans = walk_v1_intervals(env, limits, |run| {
        compare_v1_interval(
            env,
            v3,
            db,
            &mut cache,
            map,
            &run,
            limits,
            &mut written,
            &mut frontier,
            &mut accepted,
            &mut report,
        )
    })?;
    report.skipped_orphans = report.skipped_orphans.saturating_add(orphans);
    note_rocks_placements_absent_from_v1(env, v3, &mut report)?;
    compare_v1_pending(env, v3, &written, &frontier, &mut report)?;
    note_extra_rocks_offset_rows(env, v3, &written, &frontier, &accepted, &mut report)?;
    verify_content_index(v3, &mut report)?;
    note_index_audit(v3, &mut report)?;
    note_accepted_offset_ranges(accepted.range_count());
    Ok(report)
}

fn compare_v1_interval(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    map: &PlacementMap,
    run: &OpenRun,
    limits: Limits,
    written: &mut FrontierSkips,
    frontier: &mut FrontierSkips,
    accepted: &mut AcceptedOffsets,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let start = PartitionChunkOffset(run.start);
    let end = PartitionChunkOffset(run.end);
    report.intervals_checked = report.intervals_checked.saturating_add(1);
    report.tx_paths_checked = report.tx_paths_checked.saturating_add(1);
    let Some(stored) = read_v1_tx_path(env, run.tx_hash)? else {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
        note(report, "tx_path", format!("offset {}", start.0));
        return Ok(());
    };
    if tx_path_hash(&stored) != run.tx_hash {
        report.mismatch_tx_path_hash = report.mismatch_tx_path_hash.saturating_add(1);
        note(report, "tx_path_hash", format!("offset {}", start.0));
    }
    let Some(derived) = derive_at(db, cache, map, start)? else {
        report.skipped_frontier = report.skipped_frontier.saturating_add(1);
        frontier.push(start, end);
        if v3.view(|tx| tx.placement_at(start))?.is_some() {
            report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
            note(
                report,
                "interval",
                format!(
                    "offset {} is past the consensus frontier but RocksDB has a placement",
                    start.0
                ),
            );
        }
        return Ok(());
    };
    let header = db.view_eyre(|tx| tx_header_by_txid(tx, &derived.tx_id))?;
    let Some(header) = header else {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
        note(
            report,
            "tx_path",
            format!("offset {} tx header {} missing", start.0, derived.tx_id),
        );
        return Ok(());
    };
    if derived.proof.proof != stored {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
        note(report, "tx_path", format!("offset {}", start.0));
        return Ok(());
    }
    let infos = v1_infos(env, derived.data_root)?;
    let segments = tx_segments(start, end, &infos);
    let mut written_end = None;
    let mut stop_paths = false;
    for (index, (seg_start, seg_end)) in segments.into_iter().enumerate() {
        let (seg_derived, seg_header) = if index == 0 {
            (derived.clone(), header.clone())
        } else {
            match derive_at(db, cache, map, seg_start)? {
                None => {
                    note_v1_frontier_segment(v3, seg_start, end, frontier, report)?;
                    break;
                }
                Some(next) if next.data_root != derived.data_root => {
                    report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                    note(
                        report,
                        "interval",
                        format!("offset {} derives a different data root", seg_start.0),
                    );
                    stop_paths = true;
                    break;
                }
                Some(next) => {
                    let Some(next_header) =
                        db.view_eyre(|tx| tx_header_by_txid(tx, &next.tx_id))?
                    else {
                        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
                        report.mismatch_tx_path = report.mismatch_tx_path.saturating_add(1);
                        note(
                            report,
                            "tx_path",
                            format!("offset {} tx header {} missing", seg_start.0, next.tx_id),
                        );
                        stop_paths = true;
                        break;
                    };
                    (next, next_header)
                }
            }
        };
        if seg_end != seg_start {
            match derive_at(db, cache, map, seg_end)? {
                None => {
                    note_v1_frontier_segment(v3, seg_start, end, frontier, report)?;
                    break;
                }
                Some(at_end)
                    if at_end.height != seg_derived.height
                        || at_end.tx_index != seg_derived.tx_index =>
                {
                    report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                    note(
                        report,
                        "interval",
                        format!("offset {} through {}", seg_start.0, seg_end.0),
                    );
                    stop_paths = true;
                    break;
                }
                Some(_) => {}
            }
        }
        if index > 0 {
            report.intervals_checked = report.intervals_checked.saturating_add(1);
            report.tx_paths_checked = report.tx_paths_checked.saturating_add(1);
        }
        let Some(place) = v3.view(|tx| tx.placement_at(seg_start))? else {
            report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
            note(report, "interval", format!("offset {}", seg_start.0));
            stop_paths = true;
            break;
        };
        written.push(seg_start, seg_end);
        written_end = Some(seg_end);
        let tx_index = u32::try_from(seg_derived.tx_index).ok();
        let same = place.start == seg_start
            && place.end == seg_end
            && place.data_root == seg_derived.data_root
            && place.block_height == Some(seg_derived.height)
            && place.tx_index == tx_index
            && place.ledger == Some(map.ledger.get_id());
        if !same {
            report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
            note(report, "interval", format!("offset {}", seg_start.0));
        }
        if index == 0 {
            report.tx_leaf_bindings_checked = report.tx_leaf_bindings_checked.saturating_add(1);
            if let Some(binding) = read_v1_binding(env, run.tx_hash)?
                && (binding.data_root != seg_derived.data_root
                    || binding.prefix_hash != seg_header.prefix_hash)
            {
                report.tx_leaf_bindings_mismatches =
                    report.tx_leaf_bindings_mismatches.saturating_add(1);
                note(report, "tx_leaf_binding", format!("offset {}", seg_start.0));
            }
        }
        report.data_roots_checked = report.data_roots_checked.saturating_add(1);
        note_chain_extent(
            &place,
            seg_derived.tx_ledger_start,
            seg_header.data_size,
            map,
            seg_start,
            report,
        );
    }
    if let Some(path_end) = written_end
        && !stop_paths
    {
        compare_v1_paths(env, v3, start, path_end, limits, accepted, report)?;
    }
    Ok(())
}

fn note_v1_frontier_segment(
    v3: &RocksSubmoduleStore,
    seg_start: PartitionChunkOffset,
    interval_end: PartitionChunkOffset,
    frontier: &mut FrontierSkips,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    report.skipped_frontier = report.skipped_frontier.saturating_add(1);
    frontier.push(seg_start, interval_end);
    if v3.view(|tx| tx.placement_at(seg_start))?.is_some() {
        report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
        note(
            report,
            "interval",
            format!(
                "offset {} is past the consensus frontier but RocksDB has a placement",
                seg_start.0
            ),
        );
    }
    Ok(())
}

fn compare_v1_paths(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    limits: Limits,
    accepted: &mut AcceptedOffsets,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    walk_v1_path_range(env, start, end, limits, |rows| {
        let paths = read_v1_data_paths(env, &rows, true)?;
        let mut rights = Vec::with_capacity(rows.len());
        let mut metas = Vec::with_capacity(rows.len());
        for chunk in rows.chunks(v1_path_view_group()) {
            let (chunk_rights, chunk_metas) = v3.view(|tx| {
                note_verify_view();
                let rights = chunk
                    .iter()
                    .map(|(offset, _)| tx.get_data_path_by_offset(*offset))
                    .collect::<eyre::Result<Vec<_>>>()?;
                let metas = chunk
                    .iter()
                    .map(|(offset, _)| tx.get_offset_meta(*offset))
                    .collect::<eyre::Result<Vec<_>>>()?;
                Ok((rights, metas))
            })?;
            rights.extend(chunk_rights);
            metas.extend(chunk_metas);
        }
        for (index, (offset, hash)) in rows.iter().enumerate() {
            accepted.insert(offset.0);
            let path = &paths[index];
            report.offsets_checked = report.offsets_checked.saturating_add(1);
            if rights[index].as_ref() != Some(path) {
                report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                note(report, "data_path", format!("offset {}", offset.0));
            }
            // A rollback copy is always MDBX v2, never v1.
            note_mdbx_checksum(true, metas[index].as_ref(), *offset, false, report);
            if H256::from(hash_sha256(path)) != *hash {
                report.mismatch_data_path_hash = report.mismatch_data_path_hash.saturating_add(1);
                note(report, "data_path_hash", format!("offset {}", offset.0));
            }
        }
        Ok(())
    })
}

fn note_rocks_placements_absent_from_v1(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    #[cfg(test)]
    let _placement_views = CountV1PlacementViews::enter();
    let mut after = None;
    let mut cached = None;
    loop {
        let batch = v3.read_placements(after, placement_verify_batch())?;
        if batch.is_empty() {
            break;
        }
        let starts = v1_placement_starts_ok(env, &batch, &mut cached)?;
        for (place, ok) in batch.iter().zip(starts) {
            if ok {
                continue;
            }
            report.intervals_checked = report.intervals_checked.saturating_add(1);
            report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
            note(
                report,
                "interval",
                format!("offset {} is only in v3", place.start.0),
            );
        }
        after = batch.last().map(|row| row.start);
    }
    Ok(())
}

fn note_extra_rocks_offset_rows(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    written: &FrontierSkips,
    frontier: &FrontierSkips,
    accepted: &AcceptedOffsets,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut after = None;
    loop {
        let keys = v3.read_offset_row_keys(after, VERIFY_KEYS)?;
        if keys.is_empty() {
            break;
        }
        // Compared rows already proved the source hash. Read the rest once.
        let pending: Vec<PartitionChunkOffset> = keys
            .iter()
            .copied()
            .filter(|key| written.covers(key.0) && !accepted.contains(key.0))
            .collect();
        let pending_rows = v1_uncompared_hash_rows(env, &pending)?;
        for key in &keys {
            // A placement covers every offset in the tx, including one whose
            // v1 hash row has no data path. That offset is not a compared
            // path. A rocks proof or checksum there is extra.
            if written.covers(key.0) {
                if accepted.contains(key.0) {
                    continue;
                }
                let has_data_path = pending_rows
                    .iter()
                    .find(|(offset, _)| *offset == *key)
                    .and_then(|(_, row)| row.as_ref())
                    .and_then(|row| row.data_path_hash)
                    .is_some();
                if has_data_path {
                    continue;
                }
                let meta = v3.view(|tx| tx.get_offset_meta(*key))?;
                let proof = meta.as_ref().is_some_and(|row| row.proof.is_some());
                if !proof && meta.as_ref().is_none_or(|row| row.crc.is_none()) {
                    continue;
                }
                report.offsets_checked = report.offsets_checked.saturating_add(1);
                if proof {
                    report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                    report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                    note(
                        report,
                        "data_path",
                        format!("offset {} is only in v3", key.0),
                    );
                }
                // Checksum-only rows are counted here. A proof that also
                // carries a checksum is the data-path mismatch above.
                note_mdbx_checksum(false, meta.as_ref(), *key, false, report);
                continue;
            }
            if frontier.covers(key.0) {
                report.skipped_frontier = report.skipped_frontier.saturating_add(1);
                continue;
            }
            report.offsets_checked = report.offsets_checked.saturating_add(1);
            report.offset_mismatches = report.offset_mismatches.saturating_add(1);
            report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
            note(
                report,
                "data_path",
                format!("offset {} is only in v3", key.0),
            );
            let meta = v3.view(|tx| tx.get_offset_meta(*key))?;
            if meta
                .as_ref()
                .is_some_and(|row| row.proof.is_none() && row.crc.is_some())
            {
                report.mismatch_chunk_crc = report.mismatch_chunk_crc.saturating_add(1);
                note(
                    report,
                    "chunk_crc",
                    format!("offset {} checksum-only row is not in the source", key.0),
                );
            }
        }
        after = keys.last().copied();
    }
    after = None;
    loop {
        let keys = v3.read_pending_keys(after, VERIFY_KEYS)?;
        if keys.is_empty() {
            break;
        }
        for key in &keys {
            if written.covers(key.0) || frontier.covers(key.0) {
                continue;
            }
            report.pending_checked = report.pending_checked.saturating_add(1);
            report.pending_mismatches = report.pending_mismatches.saturating_add(1);
            note(
                report,
                "PendingBodyMigrationsByOffset",
                format!("offset {} is only in v3", key.0),
            );
        }
        after = keys.last().copied();
    }
    Ok(())
}

fn compare_v1_pending(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    written: &FrontierSkips,
    frontier: &FrontierSkips,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut after = None;
    loop {
        let batch = read_v1_pending_batch(env, after, VERIFY_KEYS)?;
        if batch.is_empty() {
            break;
        }
        note_rows(u64::try_from(batch.len()).unwrap_or(u64::MAX));
        for (offset, job) in &batch {
            if frontier.covers(offset.0) {
                report.skipped_frontier = report.skipped_frontier.saturating_add(1);
                continue;
            }
            if !written.covers(offset.0) {
                report.skipped_orphans = report.skipped_orphans.saturating_add(1);
                continue;
            }
            let right = v3.view(|tx| tx.get_pending_body_migration(*offset))?;
            report.pending_checked = report.pending_checked.saturating_add(1);
            if right.as_ref() != Some(job) {
                report.pending_mismatches = report.pending_mismatches.saturating_add(1);
                note(
                    report,
                    "PendingBodyMigrationsByOffset",
                    format!("offset {}", offset.0),
                );
            }
        }
        after = batch.last().map(|(key, _)| *key);
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Runs while the exclusive env is still open, before the directory is removed.
    static DURING_MDBX_INCOMPLETE_REPLACE: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
    /// Commits on the environment that still has the target open.
    static DURING_MDBX_REPLACE_ENV: std::cell::RefCell<Option<Box<dyn FnOnce(&DatabaseEnv)>>> =
        const { std::cell::RefCell::new(None) };
    /// Runs after the first lock check and before `remove_dir_all`.
    static BEFORE_REPLACE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
    /// Runs after `CONVERTING` was seen and before the replace lock is taken.
    static AFTER_CONVERTING_OBSERVED: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
    /// Runs after `remove_dir_all` and before the new directory is created.
    static AT_REPLACE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
    /// Runs after `CONVERTING` is durable. The flock is already held when
    /// [`lock_then_mark`] writes the sentinel second.
    static AFTER_CONVERTING_WRITTEN: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct MdbxReplaceHookGuard;

#[cfg(test)]
impl Drop for MdbxReplaceHookGuard {
    fn drop(&mut self) {
        DURING_MDBX_INCOMPLETE_REPLACE.with(|cell| {
            cell.borrow_mut().take();
        });
        DURING_MDBX_REPLACE_ENV.with(|cell| {
            cell.borrow_mut().take();
        });
        BEFORE_REPLACE_DELETE.with(|cell| {
            cell.borrow_mut().take();
        });
        AFTER_CONVERTING_OBSERVED.with(|cell| {
            cell.borrow_mut().take();
        });
        AT_REPLACE_DELETE.with(|cell| {
            cell.borrow_mut().take();
        });
        AFTER_CONVERTING_WRITTEN.with(|cell| {
            cell.borrow_mut().take();
        });
    }
}

#[cfg(test)]
fn set_during_mdbx_incomplete_replace(hook: impl FnOnce(&Path) + 'static) -> MdbxReplaceHookGuard {
    DURING_MDBX_INCOMPLETE_REPLACE.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

#[cfg(test)]
fn set_during_mdbx_replace_env(hook: impl FnOnce(&DatabaseEnv) + 'static) -> MdbxReplaceHookGuard {
    DURING_MDBX_REPLACE_ENV.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

/// Test hook for the gap between the schema read and the delete.
///
/// `held` is the exclusive environment that stays open through the delete.
/// A schema hook commits on that environment. The caller reads the schema
/// again after this returns.
#[cfg(test)]
fn run_mdbx_replace_hooks(path: &Path, held: &DatabaseEnv) {
    if let Some(hook) = DURING_MDBX_INCOMPLETE_REPLACE.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
    if let Some(hook) = DURING_MDBX_REPLACE_ENV.with(|cell| cell.borrow_mut().take()) {
        hook(held);
    }
}

#[cfg(test)]
fn set_before_replace_delete(hook: impl FnOnce(&Path) + 'static) -> MdbxReplaceHookGuard {
    BEFORE_REPLACE_DELETE.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

#[cfg(test)]
fn run_before_replace_delete(path: &Path) {
    if let Some(hook) = BEFORE_REPLACE_DELETE.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
}

#[cfg(test)]
fn set_after_converting_observed(hook: impl FnOnce(&Path) + 'static) -> MdbxReplaceHookGuard {
    AFTER_CONVERTING_OBSERVED.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

#[cfg(test)]
fn run_after_converting_observed(path: &Path) {
    if let Some(hook) = AFTER_CONVERTING_OBSERVED.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
}

#[cfg(test)]
fn set_at_replace_delete(hook: impl FnOnce(&Path) + 'static) -> MdbxReplaceHookGuard {
    AT_REPLACE_DELETE.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

#[cfg(test)]
fn run_at_replace_delete(path: &Path) {
    if let Some(hook) = AT_REPLACE_DELETE.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
}

#[cfg(test)]
fn set_after_converting_written(hook: impl FnOnce(&Path) + 'static) -> MdbxReplaceHookGuard {
    AFTER_CONVERTING_WRITTEN.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(hook));
    });
    MdbxReplaceHookGuard
}

#[cfg(test)]
fn run_after_converting_written(path: &Path) {
    if let Some(hook) = AFTER_CONVERTING_WRITTEN.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
}

/// Flock `LOCK`, then make `CONVERTING` durable.
///
/// The sentinel is not visible until the flock is held. A replacer that sees
/// `CONVERTING` then finds `LOCK` either busy or, after this process dies,
/// free. Closing the returned file releases the flock.
fn lock_then_mark(path: &Path) -> eyre::Result<Arc<File>> {
    let lock = rocks::acquire_conversion_lock(path)?;
    write_converting_sentinel(path)?;
    Ok(lock)
}

fn prepare_rocks_target(path: &Path, replace: bool) -> eyre::Result<Arc<File>> {
    let path = lexical_target(path)?;
    let path = path.as_path();
    rocks::refuse_unresolved_close_scratch(path)?;
    if path.exists() && !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if !path.exists() || dir_is_empty(path)? {
        // A replace that just deleted this path still holds the parent lock.
        // Creating the directory without that check would take the gap.
        // Drop the flock before the sentinel write so a replace that sees
        // CONVERTING contends on LOCK, not on this file.
        drop(hold_parent_replace_lock(path)?);
        fs::create_dir_all(path)?;
        return lock_then_mark(path);
    }
    refuse_forbidden(path)?;
    if path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "target {} contains mdbx.dat. Refusing to delete an MDBX index or a consensus database.",
            path.display()
        );
    }
    refuse_existing_rocks_schema(path)?;
    match replace_marked_incomplete(path, replace, true)? {
        ReplaceMarked::Ready(lock) => Ok(lock),
        ReplaceMarked::MdbxDat => eyre::bail!(
            "target {} contains mdbx.dat. Refusing to delete an MDBX index or a consensus database.",
            path.display()
        ),
    }
}

fn prepare_mdbx_target(
    path: &Path,
    args: DatabaseArguments,
    replace: bool,
) -> eyre::Result<Arc<File>> {
    let path = lexical_target(path)?;
    let path = path.as_path();
    rocks::refuse_unresolved_close_scratch(path)?;
    if path.exists() && !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if !path.exists() || dir_is_empty(path)? {
        // Same gap as the RocksDB prepare: the parent lock covers the
        // directory that does not exist yet. Do not hold it across the
        // sentinel write.
        drop(hold_parent_replace_lock(path)?);
        fs::create_dir_all(path)?;
        return lock_then_mark(path);
    }
    // SCHEMA beside CONVERTING is a finished index. Refuse it before any delete.
    refuse_existing_rocks_schema(path)?;
    refuse_forbidden(path)?;
    if path.join("mdbx.dat").is_file() {
        return replace_incomplete_mdbx(path, args, replace);
    }
    match replace_marked_incomplete(path, replace, false)? {
        ReplaceMarked::Ready(lock) => Ok(lock),
        // `mdbx.dat` appeared after the first stat. Open it. Do not delete first.
        ReplaceMarked::MdbxDat => replace_incomplete_mdbx(path, args, replace),
    }
}

/// Delete an incomplete MDBX target while its exclusive env is held.
///
/// The env stays open from the schema read through `remove_dir_all`. A current
/// row is not deleted. Drop the env before creating the new directory: close
/// must not write into the directory this function is about to create.
fn replace_incomplete_mdbx(
    path: &Path,
    args: DatabaseArguments,
    replace: bool,
) -> eyre::Result<Arc<File>> {
    let args = args.with_exclusive(Some(true));
    let env = match DatabaseEnv::open(path, DatabaseEnvKind::RW, args) {
        Ok(env) => env,
        Err(err) if is_lock_error(&eyre::eyre!("{err}")) => {
            return Err(map_lock(eyre::eyre!("{err}")));
        }
        Err(err) => {
            eyre::bail!(
                "target {} could not be opened ({err}). Refusing to delete it.",
                path.display()
            );
        }
    };
    refuse_mdbx_replace_schema(path, &env)?;
    if !migrate::converting_sentinel_present(path)? {
        eyre::bail!(
            "target {} has no CONVERTING sentinel. Pass --replace-incomplete only deletes a directory this tool marked. Delete an older incomplete directory by hand.",
            path.display()
        );
    }
    if !replace {
        eyre::bail!(
            "target {} is an incomplete conversion. Pass --replace-incomplete to delete it and start over.",
            path.display()
        );
    }
    refuse_existing_rocks_schema(path)?;
    refuse_forbidden(path)?;
    #[cfg(test)]
    run_mdbx_replace_hooks(path, &env);
    refuse_mdbx_replace_schema(path, &env)?;
    fs::remove_dir_all(path).wrap_err("remove incomplete target")?;
    drop(env);
    fs::create_dir_all(path)?;
    lock_then_mark(path)
}

/// Refuse a schema row that `--replace-incomplete` must not delete.
fn refuse_mdbx_replace_schema(path: &Path, env: &DatabaseEnv) -> eyre::Result<()> {
    let schema = env.view_eyre(read_schema_bytes)?;
    match schema.as_deref() {
        Some(bytes) if bytes == SUBMODULE_SCHEMA.as_bytes() => {
            eyre::bail!(
                "target {} is a completed MDBX index. Refusing to replace it.",
                path.display()
            );
        }
        Some(bytes) if bytes == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() => Ok(()),
        Some(_) => {
            eyre::bail!(
                "target {} schema row is not an incomplete submodule conversion. Refusing to delete a consensus database or other index.",
                path.display()
            );
        }
        None if mdbx_has_v1_rows(env)? => {
            eyre::bail!(
                "target {} is a schema v1 index. Refusing to delete it.",
                path.display()
            );
        }
        None => Ok(()),
    }
}

#[derive(Debug)]
enum ReplaceMarked {
    /// The old directory is gone. `LOCK` is flocked and `CONVERTING` is durable.
    Ready(Arc<File>),
    /// `mdbx.dat` is present. The caller opens that environment and does not
    /// delete the directory first.
    MdbxDat,
}

/// Exclusive flock in the parent directory.
///
/// `remove_dir_all` of the target unlinks `LOCK`, so that flock does not
/// cover the directory created afterwards. This file is not inside the
/// target. It is held from the sentinel re-check through [`lock_then_mark`].
fn hold_parent_replace_lock(path: &Path) -> eyre::Result<File> {
    use std::os::fd::AsRawFd as _;

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let lock_path = parent.join(format!(".{name}.replace-lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .wrap_err("open replace lock")?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        eyre::bail!(
            "refusing to run because another --replace-incomplete holds the directory, so a conversion may be running. Stop that process and retry. {err}"
        );
    }
    Ok(file)
}

fn bail_no_converting_sentinel(path: &Path) -> eyre::Report {
    eyre::eyre!(
        "target {} has no CONVERTING sentinel. Pass --replace-incomplete only deletes a directory this tool marked. Delete an older incomplete directory by hand.",
        path.display()
    )
}

fn replace_marked_incomplete(
    path: &Path,
    replace: bool,
    refuse_mdbx: bool,
) -> eyre::Result<ReplaceMarked> {
    rocks::refuse_unresolved_close_scratch(path)?;
    if !migrate::converting_sentinel_present(path)? {
        return Err(bail_no_converting_sentinel(path));
    }
    if !replace {
        eyre::bail!(
            "target {} is an incomplete conversion. Pass --replace-incomplete to delete it and start over.",
            path.display()
        );
    }
    // A second process can already be past the sentinel check above. It must
    // not delete on that observation: `remove_dir_all` drops LOCK, and the
    // directory created below is a different inode.
    #[cfg(test)]
    run_after_converting_observed(path);
    let _parent_lock = hold_parent_replace_lock(path)?;
    // The parent lock does not wait. The directory can change before the
    // non-blocking acquire, so read the sentinel again.
    if !migrate::converting_sentinel_present(path)? {
        return Err(bail_no_converting_sentinel(path));
    }
    // Hold LOCK across the marker check and the delete. Seal writes SCHEMA
    // before it drops that lock. `Ok(None)` means LOCK is absent. Do not
    // delete on that result until it is read again under the parent lock.
    let mut hold = rocks::hold_rocks_lock(path)?;
    refuse_existing_rocks_schema(path)?;
    refuse_forbidden(path)?;
    if refuse_mdbx && path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "target {} contains mdbx.dat. Refusing to delete an MDBX index or a consensus database.",
            path.display()
        );
    }
    #[cfg(test)]
    run_before_replace_delete(path);
    // `LOCK`, `CURRENT`, or `mdbx.dat` may have appeared since the first stat.
    if path.join("mdbx.dat").is_file() {
        if refuse_mdbx {
            eyre::bail!(
                "target {} contains mdbx.dat. Refusing to delete an MDBX index or a consensus database.",
                path.display()
            );
        }
        return Ok(ReplaceMarked::MdbxDat);
    }
    if hold.is_none() {
        hold = rocks::hold_rocks_lock(path)?;
    }
    if path.join("CURRENT").is_file() && hold.is_none() {
        eyre::bail!(
            "target {} has CURRENT but no LOCK file. Refusing to delete it.",
            path.display()
        );
    }
    if !migrate::converting_sentinel_present(path)? {
        return Err(bail_no_converting_sentinel(path));
    }
    // The parent lock is held through `lock_then_mark`, so another replace
    // cannot recreate this directory after `remove_dir_all`. A CONVERTING
    // file with no LOCK is a dropped incomplete directory and is removed.
    // A close does not take this lock. It can park the directory after the
    // earlier scratch check. Read the marker again before the delete.
    rocks::refuse_unresolved_close_scratch(path)?;
    fs::remove_dir_all(path).wrap_err("remove incomplete target")?;
    drop(hold);
    // The directory is gone. The parent lock is what stops another replace
    // from creating it again before `lock_then_mark`.
    #[cfg(test)]
    run_at_replace_delete(path);
    fs::create_dir_all(path)?;
    Ok(ReplaceMarked::Ready(lock_then_mark(path)?))
}

fn refuse_forbidden(path: &Path) -> eyre::Result<()> {
    for name in FORBIDDEN_NAMES {
        if path.join(name).exists() {
            eyre::bail!(
                "target {} contains {name}. Refusing to delete a storage module.",
                path.display()
            );
        }
    }
    Ok(())
}

fn write_converting_sentinel(path: &Path) -> eyre::Result<()> {
    let file_path = path.join(CONVERTING_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file_path)
        .wrap_err_with(|| format!("create {}", file_path.display()))?;
    file.write_all(SUBMODULE_CONVERT_INCOMPLETE.as_bytes())?;
    file.sync_all()?;
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .wrap_err("sync target directory")?;
    #[cfg(test)]
    run_after_converting_written(path);
    Ok(())
}

/// Drop `CONVERTING` only after the completion marker is durable.
///
/// A crash before this call leaves the sentinel, so `--replace-incomplete`
/// can still delete the partial directory. A crash after the marker and
/// before this call leaves both. The marker still refuses replacement.
fn clear_converting_sentinel(path: &Path) -> eyre::Result<()> {
    let file_path = path.join(CONVERTING_FILE);
    if !file_path.is_file() {
        return Ok(());
    }
    fs::remove_file(&file_path).wrap_err("remove CONVERTING")?;
    // MDBX has no CURRENT. Drop the flock file after CONVERTING so a finished
    // directory does not keep it. RocksDB keeps LOCK beside CURRENT.
    if !path.join("CURRENT").is_file() {
        let lock_path = path.join("LOCK");
        if lock_path.is_file() {
            fs::remove_file(&lock_path).wrap_err("remove conversion LOCK")?;
        }
    }
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .wrap_err("sync target directory")?;
    Ok(())
}

fn verify_empty_and_rocks(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
) -> eyre::Result<EngineEquivalenceReport> {
    verify_reset_target(reset_marker_on(env)?, v3)
}

fn verify_reset_target(
    source_reset: bool,
    v3: &RocksSubmoduleStore,
) -> eyre::Result<EngineEquivalenceReport> {
    let mut report = EngineEquivalenceReport::default();
    note_reset(&mut report, v3.reset_in_progress()? != source_reset);
    note_schema(
        &mut report,
        v3.schema_bytes()?.as_deref() == Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()),
    );
    if !v3.read_placements(None, 1)?.is_empty() {
        report.intervals_mismatches = 1;
        note(&mut report, "interval", "reset or empty source".to_owned());
    }
    if !v3.read_offset_row_keys(None, 1)?.is_empty() {
        report.offset_mismatches = 1;
        report.mismatch_data_path = 1;
        note(&mut report, "data_path", "reset or empty source".to_owned());
    }
    if !v3.read_pending_keys(None, 1)?.is_empty() {
        report.pending_mismatches = 1;
        note(
            &mut report,
            "PendingBodyMigrationsByOffset",
            "reset or empty source".to_owned(),
        );
    }
    verify_content_index(v3, &mut report)?;
    note_index_audit(v3, &mut report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use eyre::WrapErr as _;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        DataLedger, DataTransactionHeader, DataTransactionLedger, DatabaseVersion, H256, H256List,
        IrysBlockHeader, PartitionChunkOffset, RelativeChunkOffset, hash_sha256,
    };
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::{DatabaseEnv, DatabaseEnvKind};

    use super::super::store::{
        MdbxSubmoduleStore, SubmoduleStore, mdbx_submodule_views, reset_mdbx_submodule_views,
    };
    use super::super::tables::{
        ChunkPathHashes, DataRootInfo, DataRootInfos, PackedOffsetRow, PendingBodyMigration,
        SUBMODULE_SCHEMA_ROCKS_V1, TxLeafBinding,
    };
    use super::{
        ConvertOptions, ConvertStats, accepted_offset_ranges, convert_mdbx_to_rocks,
        convert_mdbx_to_rocks_and_verify, convert_rocks_to_mdbx, reset_accepted_offset_ranges,
        reset_v1_copy_path_views, reset_v1_data_path_reads, reset_v1_range_copy_batches,
        reset_v1_uncompared_hash_views, reset_verify_placement_caches, reset_verify_views,
        set_merge_batch, set_v1_path_view_group, set_v1_placement_batch,
        set_v1_uncompared_hash_group, v1_copy_path_views, v1_data_path_reads, v1_placement_views,
        v1_range_copy_batches, v1_uncompared_hash_views, verify_mdbx_and_rocks,
        verify_placement_caches, verify_rocks_records, verify_views,
    };
    use crate::db::IrysDatabaseExt as _;
    use crate::submodule::rocks::RocksSubmoduleStore;
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
            crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
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
        let data_0 = indexed_data_leaf(1);
        let data_3 = indexed_data_leaf(3);
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
        Ok(())
    }

    fn populate_mdbx(root: &Path) -> eyre::Result<PathBuf> {
        let consensus = root.join("consensus");
        write_chain(&consensus)?;
        let store = MdbxSubmoduleStore::open(root.join("db"), testing_args()?)?;
        write_index(&store, &consensus)?;
        Ok(consensus)
    }

    /// A schema v2 index plus the consensus chain the converter derives from.
    struct Populated {
        root: tempfile::TempDir,
        consensus: PathBuf,
        mdbx: PathBuf,
        rocks: PathBuf,
        options: ConvertOptions,
    }

    fn populated(prefix: &str) -> eyre::Result<Populated> {
        let root = temp(prefix);
        let consensus = populate_mdbx(root.path())?;
        let options = options_for(&consensus)?;
        Ok(Populated {
            mdbx: root.path().join("db"),
            rocks: root.path().join("rocks"),
            consensus,
            options,
            root,
        })
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
                Some(indexed_data_leaf(1))
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(3))?,
                Some(indexed_data_leaf(3))
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
        Ok(())
    }

    fn assert_rocks(store: &impl SubmoduleStore, consensus: &Path) -> eyre::Result<()> {
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
                Some(indexed_data_leaf(1))
            );
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(3))?,
                Some(indexed_data_leaf(3))
            );
            assert_eq!(
                tx.offsets_for_content_hash(H256([1_u8; 32]))?,
                vec![PartitionChunkOffset(0)]
            );
            assert_eq!(
                tx.offsets_for_content_hash(H256([3_u8; 32]))?,
                vec![PartitionChunkOffset(3)]
            );
            assert!(tx.offsets_for_content_hash(H256([9_u8; 32]))?.is_empty());
            let job = tx
                .get_pending_body_migration(PartitionChunkOffset(0))?
                .expect("pending");
            assert_eq!(job.data_root, pair.root_a);
            Ok(())
        })?;
        assert!(!store.reset_in_progress()?);
        Ok(())
    }

    fn assert_copy_counts(stats: &super::ConvertStats) {
        assert_eq!(stats.tx_paths, 2);
        assert_eq!(stats.tx_leaf_bindings, 2);
        assert_eq!(stats.intervals, 2);
        assert_eq!(stats.path_hashes, 2);
        assert_eq!(stats.data_paths, 2);
        assert_eq!(stats.data_roots, 2);
        assert_eq!(stats.pending, 1);
        assert!(!stats.reset_marker);
        assert!(!stats.migrated_v1);
        assert!(!stats.source_v1);
    }

    /// Coverage of one row batch is one source view. One interval reads its tx
    /// path, leaf binding, and data-root infos in one view. A one-row batch is
    /// one coverage view per row, and the rows are still copied.
    #[test]
    fn mdbx_to_rocks_reads_coverage_and_placement_in_batch_views() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-v2-batch-views")?;
        {
            let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
            store.update(|tx| {
                tx.add_full_data_path(PartitionChunkOffset(4), indexed_data_leaf(9))
            })?;
        }
        let mut dry = options.clone();
        dry.dry_run = true;
        reset_mdbx_submodule_views();
        let preview = convert_mdbx_to_rocks(&mdbx, &rocks, &dry)?;
        assert_eq!(preview.intervals, 2, "{preview:?}");
        assert_eq!(preview.data_paths, 2, "{preview:?}");
        assert_eq!(preview.pending, 1, "{preview:?}");
        assert_eq!(preview.skipped_orphans, 1, "{preview:?}");
        assert_eq!(
            mdbx_submodule_views(),
            4,
            "two intervals, one data-path batch, and one pending batch; got {}",
            mdbx_submodule_views()
        );

        reset_mdbx_submodule_views();
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.data_paths, 2, "{stats:?}");
        assert_eq!(stats.pending, 1, "{stats:?}");
        assert_eq!(stats.skipped_orphans, 1, "{stats:?}");
        assert_eq!(mdbx_submodule_views(), 4, "{stats:?}");
        let opened = RocksSubmoduleStore::open(&rocks)?;
        assert!(
            opened
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset(0)))?
                .is_some()
        );
        assert!(
            opened
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset(4)))?
                .is_none(),
            "an offset outside every interval is not copied"
        );

        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            mut options,
        } = populated("convert-v2-batch-views-one")?;
        options.batch_rows = 1;
        reset_mdbx_submodule_views();
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.intervals, 2, "{stats:?}");
        assert_eq!(stats.data_paths, 2, "{stats:?}");
        assert_eq!(stats.pending, 1, "{stats:?}");
        assert_eq!(stats.skipped_orphans, 0, "{stats:?}");
        assert_eq!(
            mdbx_submodule_views(),
            5,
            "a batch of one is one coverage view per row, and each interval is one view"
        );
        Ok(())
    }

    /// A sealed conversion may leave a header-sized log. It must not leave a
    /// recycled write-ahead log.
    const WAL_BYTE_BOUND: u64 = 4 * 1024;

    fn wal_snapshot(dir: &Path) -> (u64, u64, usize) {
        fn walk(dir: &Path, dir_bytes: &mut u64, wal_bytes: &mut u64, wal_files: &mut usize) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.is_dir() {
                    walk(&entry.path(), dir_bytes, wal_bytes, wal_files);
                    continue;
                }
                let len = meta.len();
                *dir_bytes = dir_bytes.saturating_add(len);
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let stem = name.strip_suffix(".log").unwrap_or("");
                if !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit()) {
                    *wal_bytes = wal_bytes.saturating_add(len);
                    *wal_files = wal_files.saturating_add(1);
                }
            }
        }
        let mut dir_bytes = 0;
        let mut wal_bytes = 0;
        let mut wal_files = 0;
        walk(dir, &mut dir_bytes, &mut wal_bytes, &mut wal_files);
        (dir_bytes, wal_bytes, wal_files)
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
        assert_eq!(report.content_offset_mismatches, 0, "{report}");
        assert!(report.content_offsets_checked >= 2, "{report}");
    }

    /// 64-byte data-proof leaf. The first 32 bytes are the content hash.
    fn indexed_data_leaf(byte: u8) -> Vec<u8> {
        let mut leaf = vec![0_u8; 64];
        leaf[..32].fill(byte);
        leaf
    }

    #[test]
    fn convert_round_trip_mdbx_rocks_mdbx() -> eyre::Result<()> {
        let rss_before = vm_kib("VmRSS:");
        let Populated {
            root,
            consensus,
            mdbx,
            rocks,
            options,
        } = populated("convert-round")?;
        let back = root.path().join("db-back");

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let (dir_bytes, wal_bytes, wal_files) = wal_snapshot(&rocks);
        assert!(
            wal_bytes <= WAL_BYTE_BOUND,
            "dir_bytes={dir_bytes} wal_bytes={wal_bytes} wal_files={wal_files}"
        );
        assert!(!stats.migrated_v1);
        assert_copy_counts(&stats);
        assert_eq!(
            fs::read_to_string(rocks.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_ROCKS_V1
        );
        assert!(!rocks.join("CONVERTING").exists());
        assert_equivalent(&verify_mdbx_and_rocks(&mdbx, &rocks, &options)?);
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            assert_rocks(&opened, &consensus)?;
        }
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;

        let back_stats = convert_rocks_to_mdbx(&rocks, &back, &options)?;
        assert_copy_counts(&back_stats);
        assert!(!back.join("CONVERTING").exists());
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
        eprintln!(
            "convert_round_trip VmRSS_KiB={rss_after} VmHWM_KiB={hwm} growth_KiB={growth} dir_bytes={dir_bytes} wal_bytes={wal_bytes} wal_files={wal_files}"
        );
        assert!(hwm > 0, "VmHWM was not readable");
        // VmRSS is the whole process. A parallel `cargo test` counts other
        // tests that open a RocksDB v1 index while this one runs (the
        // per-family verify tests open several at once). The round trip on
        // its own is far below this bound.
        assert!(growth < 1024 * 1024, "round trip grew RSS by {growth} KiB");
        Ok(())
    }

    #[test]
    fn convert_writes_the_packing_record() -> eyre::Result<()> {
        use crate::metadata::MetadataKey;
        use crate::submodule::PackingRecord;

        let root = temp("convert-packing-record");
        let consensus = root.path().join("consensus");
        write_chain(&consensus)?;
        let mdbx = root.path().join("db");
        {
            let _store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
        }
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        let record = PackingRecord {
            packing_address: [0x22; 20],
            iterations: 1000,
            chunk_size: options.chunk_size,
            chain_id: 1270,
        };
        options.packing_record = Some(record);
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let opened = RocksSubmoduleStore::open(&rocks)?;
        let stored = opened
            .read_raw_metadata(MetadataKey::PackingRecord)?
            .expect("packing record");
        assert_eq!(stored.as_slice(), record.encode().as_slice());
        Ok(())
    }

    #[test]
    fn convert_interrupt_leaves_no_marker_and_can_be_redone() -> eyre::Result<()> {
        let Populated {
            root,
            consensus,
            mdbx,
            rocks,
            options,
        } = populated("convert-interrupt")?;
        let mut broken = options.clone();
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
        assert_equivalent(&verify_mdbx_and_rocks(&mdbx, &rocks, &options)?);

        let mdbx_back = root.path().join("db-back");
        broken.fail_after_batches = Some(1);
        broken.replace_incomplete = false;
        let err = convert_rocks_to_mdbx(&rocks, &mdbx_back, &broken).unwrap_err();
        assert!(format!("{err:#}").contains("injected"), "{err:#}");
        assert_eq!(
            fs::read_to_string(rocks.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_ROCKS_V1
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
    fn foreign_marker_is_not_replaced() -> eyre::Result<()> {
        let root = temp("convert-foreign-marker");
        let path = root.path().join("rocks");
        fs::create_dir_all(&path)?;
        let marker = b"foreign-layout\n";
        fs::write(path.join("SCHEMA"), marker)?;
        fs::write(path.join("CURRENT"), b"MANIFEST-000001\n")?;
        let err = super::prepare_rocks_target(&path, false).unwrap_err();
        assert!(err.to_string().contains("existing index schema"), "{err}");
        let err = super::prepare_rocks_target(&path, true).unwrap_err();
        assert!(err.to_string().contains("existing index schema"), "{err}");
        assert_eq!(fs::read(path.join("SCHEMA"))?, marker);
        assert!(path.join("CURRENT").is_file());
        Ok(())
    }

    #[test]
    fn convert_refuses_same_path_and_non_empty_target() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus,
            mdbx,
            rocks,
            options,
        } = populated("convert-refuse-target")?;
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
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-mismatch")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                // Offset 0 is inside a placement. An uncovered offset is dropped.
                tx.add_full_data_path(PartitionChunkOffset(0), b"extra".to_vec())
            })?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.mismatch_data_path >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn verify_counts_a_derived_path_mismatch() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-verify-path")?;
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
        assert!(report.mismatch_tx_path >= 1, "{report}");
        assert!(report.mismatch_tx_path_hash >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn convert_aborts_when_stored_bytes_differ() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-abort-bytes")?;
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
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("partition offset"), "{message}");
        assert!(!rocks.join("SCHEMA").exists());
        Ok(())
    }

    #[test]
    fn convert_reads_v1_without_writing_the_source() -> eyre::Result<()> {
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
            tx.put::<master::DataRootInfosByDataRoot>(
                pair.root_a,
                DataRootInfos(vec![DataRootInfo {
                    start_offset: RelativeChunkOffset(-8),
                    data_size: 352,
                }]),
            )?;
            tx.commit()?;
        }
        drop(db);

        let options = options_for(&consensus)?;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert!(!stats.migrated_v1);
        assert!(stats.source_v1);
        assert_eq!(stats.intervals, 1);
        assert_eq!(stats.tx_leaf_bindings, 1);
        assert_eq!(stats.path_hashes, 1);
        assert_eq!(stats.data_roots, 1);
        assert_eq!(stats.data_paths, 1);
        {
            let opened = DatabaseEnv::open(
                &mdbx,
                DatabaseEnvKind::RO,
                testing_args()?.with_exclusive(Some(true)),
            )
            .map_err(|err| eyre::eyre!("reopen v1 source: {err}"))?;
            opened.view_eyre(|tx| {
                use crate::metadata::MetadataKey;
                use crate::submodule::tables::Metadata;
                use reth_db::transaction::DbTx as _;
                let schema = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
                assert!(schema.is_none(), "v1 source schema row was written");
                let kept = tx.get::<master::ChunkDataPathByPathHash>(data_hash)?;
                assert_eq!(kept.as_deref(), Some(data.as_slice()));
                Ok(())
            })?;
        }
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
            assert_eq!(place.tx_start_offset, RelativeChunkOffset(-8));
            assert_eq!(place.data_size, 352);
            assert_eq!(place.start, PartitionChunkOffset(0));
            assert_eq!(place.end, PartitionChunkOffset(0));
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn replace_refuses_storage_files_consensus_v1_and_a_held_lock() -> eyre::Result<()> {
        let Populated {
            root,
            consensus,
            mdbx,
            options,
            ..
        } = populated("convert-refuse-delete")?;

        for name in [
            "chunks.dat",
            "chunks.crc",
            "packing_params.toml",
            "intervals.json",
        ] {
            let rocks = root.path().join(name);
            fs::create_dir_all(&rocks)?;
            fs::write(rocks.join(name), b"keep")?;
            fs::write(
                rocks.join("CONVERTING"),
                super::SUBMODULE_CONVERT_INCOMPLETE,
            )?;
            let mut flagged = options.clone();
            flagged.replace_incomplete = true;
            let err = convert_mdbx_to_rocks(&mdbx, &rocks, &flagged).unwrap_err();
            assert!(format!("{err:#}").contains(name), "{name}: {err:#}");
            assert_eq!(fs::read(rocks.join(name))?, b"keep");
        }

        let foreign = root.path().join("foreign");
        fs::create_dir_all(&foreign)?;
        {
            let db = open_or_create_db(&foreign, IrysTables::ALL, testing_args()?)?;
            db.update_eyre(|tx| {
                crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
                Ok(())
            })?;
        }
        fs::write(
            foreign.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        let mut flagged = options.clone();
        flagged.replace_incomplete = true;
        let err = convert_rocks_to_mdbx(
            &{
                let rocks = root.path().join("rocks-src");
                convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
                rocks
            },
            &foreign,
            &flagged,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("consensus") || format!("{err:#}").contains("schema row"),
            "{err:#}"
        );
        assert!(foreign.join("mdbx.dat").is_file());

        let err = convert_mdbx_to_rocks(&mdbx, &consensus, &options).unwrap_err();
        assert!(format!("{err:#}").contains("consensus"), "{err:#}");
        assert!(consensus.join("mdbx.dat").is_file());

        let nested = mdbx.join("nested");
        let err = convert_mdbx_to_rocks(&mdbx, &nested, &options).unwrap_err();
        assert!(format!("{err:#}").contains("overlap"), "{err:#}");

        let v1 = root.path().join("v1-target");
        fs::create_dir_all(&v1)?;
        {
            use crate::submodule::tables::v1_fixture::master;
            use reth_db::Database as _;
            use reth_db::transaction::{DbTx as _, DbTxMut as _};
            let db = open_or_create_db(&v1, master::MasterV1Tables::ALL, testing_args()?)?;
            let tx = db.tx_mut()?;
            tx.put::<master::ChunkPathHashesByOffset>(
                PartitionChunkOffset(0),
                ChunkPathHashes {
                    data_path_hash: None,
                    tx_path_hash: Some(hid(9)),
                },
            )?;
            tx.commit()?;
        }
        fs::write(v1.join("CONVERTING"), super::SUBMODULE_CONVERT_INCOMPLETE)?;
        let err = convert_rocks_to_mdbx(&root.path().join("rocks-src"), &v1, &flagged).unwrap_err();
        assert!(format!("{err:#}").contains("schema v1"), "{err:#}");
        assert!(v1.join("mdbx.dat").is_file());

        let completed = root.path().join("completed-v3");
        fs::create_dir_all(&completed)?;
        fs::write(completed.join("SCHEMA"), SUBMODULE_SCHEMA_ROCKS_V1)?;
        fs::write(
            completed.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        let err = super::prepare_rocks_target(&completed, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");
        assert!(completed.join("SCHEMA").is_file());

        let completed_v2 = root.path().join("completed-v2");
        fs::create_dir_all(&completed_v2)?;
        fs::write(completed_v2.join("SCHEMA"), super::SUBMODULE_SCHEMA)?;
        let err = super::prepare_rocks_target(&completed_v2, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");

        let unmarked = root.path().join("unmarked");
        fs::create_dir_all(&unmarked)?;
        fs::write(unmarked.join("note"), b"keep")?;
        let err = super::prepare_rocks_target(&unmarked, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("--replace-incomplete"),
            "{err:#}"
        );
        assert_eq!(fs::read(unmarked.join("note"))?, b"keep");

        let held = root.path().join("held");
        fs::create_dir_all(&held)?;
        fs::write(held.join("CONVERTING"), super::SUBMODULE_CONVERT_INCOMPLETE)?;
        let lock_file = std::fs::File::create(held.join("LOCK"))?;
        let rc = unsafe {
            use std::os::fd::AsRawFd as _;
            libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
        };
        assert_eq!(rc, 0);
        let err = super::prepare_rocks_target(&held, true).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        assert!(held.join("CONVERTING").is_file());
        Ok(())
    }

    /// The sentinel was observed, then another process removed that directory
    /// and created a new one before this process took a lock. The new
    /// directory has no `CONVERTING` and no `LOCK`. It must not be deleted.
    #[test]
    fn replace_after_the_sentinel_check_does_not_delete_a_recreated_directory() -> eyre::Result<()>
    {
        if std::env::var_os("IRYS_REPLACE_OBSERVED_CHILD").is_some() {
            return replace_observed_child();
        }
        let root = temp("convert-replace-observed");
        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("LOCK"), b"")?;
        fs::write(rocks.join("KEEP"), b"old")?;
        let mut child = Command::new(std::env::current_exe().expect("test bin"))
            .arg("replace_after_the_sentinel_check_does_not_delete_a_recreated_directory")
            .env("IRYS_REPLACE_OBSERVED_CHILD", "1")
            .env("IRYS_REPLACE_DIR", &rocks)
            .env("IRYS_REPLACE_ROOT", root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("spawn replace child")?;
        let paused = root.path().join("paused");
        let start = Instant::now();
        while !paused.exists() {
            if start.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                eyre::bail!("child did not reach the sentinel check");
            }
            thread::sleep(Duration::from_millis(20));
        }
        fs::remove_dir_all(&rocks)?;
        fs::create_dir_all(&rocks)?;
        fs::write(rocks.join("NEWKEEP"), b"new")?;
        fs::write(root.path().join("go"), b"1")?;
        let output = child.wait_with_output().wrap_err("wait replace child")?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            rocks.join("NEWKEEP").is_file(),
            "recreated directory was deleted: {text}"
        );
        assert_eq!(fs::read(rocks.join("NEWKEEP"))?, b"new");
        assert!(
            output.status.success(),
            "child did not stop on the recreated directory: {text}"
        );
        Ok(())
    }

    fn replace_observed_child() -> eyre::Result<()> {
        let rocks = PathBuf::from(std::env::var("IRYS_REPLACE_DIR")?);
        let root = PathBuf::from(std::env::var("IRYS_REPLACE_ROOT")?);
        let paused = root.join("paused");
        let go = root.join("go");
        let _guard = super::set_after_converting_observed(move |_dir| {
            fs::write(&paused, b"1").expect("pause file");
            let start = Instant::now();
            while !go.exists() {
                if start.elapsed() > Duration::from_secs(10) {
                    panic!("timed out waiting to resume replace");
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let err = super::prepare_rocks_target(&rocks, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("CONVERTING"),
            "recreated directory was eligible for delete: {text}"
        );
        assert!(rocks.join("NEWKEEP").is_file(), "{text}");
        Ok(())
    }

    /// One replace is past its checks and has not deleted yet. A second
    /// `--replace-incomplete` must not remove that directory.
    #[test]
    fn second_replace_does_not_delete_while_the_first_holds_the_directory() -> eyre::Result<()> {
        if std::env::var_os("IRYS_REPLACE_SECOND_CHILD").is_some() {
            let path = PathBuf::from(std::env::var("IRYS_REPLACE_DIR")?);
            super::prepare_rocks_target(&path, true)?;
            return Ok(());
        }
        let root = temp("convert-replace-second");
        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let flag = std::sync::Arc::clone(&recorded);
        let _guard = super::set_at_replace_delete(move |dir| {
            let output = Command::new(std::env::current_exe().expect("test bin"))
                .arg("second_replace_does_not_delete_while_the_first_holds_the_directory")
                .env("IRYS_REPLACE_SECOND_CHILD", "1")
                .env("IRYS_REPLACE_DIR", dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("spawn second replace");
            *flag.lock().expect("second replace") = Some(output);
        });
        let prepared = super::prepare_rocks_target(&rocks, true);
        let output = recorded
            .lock()
            .expect("second replace")
            .take()
            .expect("hook ran");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "second replace deleted the directory the first process still owns: {text}"
        );
        assert!(
            text.contains("may be running"),
            "second replace did not stop on the held directory: {text}"
        );
        prepared?;
        assert!(!rocks.join("KEEP").is_file());
        assert!(rocks.join("CONVERTING").is_file());
        assert!(rocks.join("LOCK").is_file());
        Ok(())
    }

    /// `CONVERTING` without `LOCK` is a dropped incomplete directory.
    /// `--replace-incomplete` still deletes it and starts again.
    #[test]
    fn replace_incomplete_deletes_converting_without_a_lock_file() -> eyre::Result<()> {
        let root = temp("convert-replace-no-lock");
        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        super::prepare_rocks_target(&rocks, true)?;
        assert!(!rocks.join("KEEP").is_file());
        assert!(rocks.join("CONVERTING").is_file());
        assert!(rocks.join("LOCK").is_file());
        assert!(!rocks.join("SCHEMA").is_file());
        Ok(())
    }

    /// A dropped conversion holds no lock and has no SCHEMA file.
    /// `--replace-incomplete` deletes that directory and starts again.
    #[test]
    fn replace_incomplete_deletes_a_dropped_converting_directory() -> eyre::Result<()> {
        let root = temp("convert-replace-dropped");
        let rocks = root.path().join("rocks");
        super::prepare_rocks_target(&rocks, false)?;
        drop(RocksSubmoduleStore::open_converting(&rocks)?);
        fs::write(rocks.join("KEEP"), b"keep")?;
        assert!(rocks.join("CONVERTING").is_file());
        assert!(!rocks.join("SCHEMA").is_file());
        super::prepare_rocks_target(&rocks, true)?;
        assert!(!rocks.join("KEEP").is_file());
        assert!(rocks.join("CONVERTING").is_file());
        assert!(!rocks.join("SCHEMA").is_file());
        Ok(())
    }

    /// SCHEMA can land after the earlier look and before the delete.
    /// The delete must see that marker and leave the directory in place.
    #[test]
    fn replace_incomplete_keeps_a_schema_seen_under_the_lock() -> eyre::Result<()> {
        let root = temp("convert-replace-schema-recheck");
        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(rocks.join("SCHEMA"), SUBMODULE_SCHEMA_ROCKS_V1)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let err = super::replace_marked_incomplete(&rocks, true, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(rocks.join("SCHEMA").is_file());
        Ok(())
    }

    fn seal_with_lock_removed(
        label: &str,
        stop: Option<crate::submodule::rocks::CloseScratchStop>,
    ) -> eyre::Result<(tempfile::TempDir, PathBuf, String)> {
        let root = temp(label);
        let rocks = root.path().join("rocks");
        super::prepare_rocks_target(&rocks, false)?;
        let opened = RocksSubmoduleStore::open_converting(&rocks)?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        // The open fd still holds the LOCK inode. The path no longer matches
        // it, so seal refuses and parks the directory.
        fs::remove_file(rocks.join("LOCK"))?;
        let _guard = stop.map(crate::submodule::rocks::set_close_scratch_stop);
        let err = opened.seal_and_stamp().unwrap_err();
        drop(_guard);
        Ok((root, rocks, format!("{err:#}")))
    }

    /// Marker file, or the parked sibling a kill left with no marker.
    fn close_scratch_location(root: &Path) -> Option<PathBuf> {
        let marker = root.join(".rocks.close-scratch-marker");
        if marker.is_file() {
            let Ok(text) = fs::read_to_string(&marker) else {
                return Some(marker);
            };
            let name = text.trim();
            if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
                return Some(root.join(name));
            }
        }
        let entries = fs::read_dir(root).ok()?;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains(".close-scratch-") && entry.path().is_dir() {
                return Some(entry.path());
            }
        }
        None
    }

    fn assert_open_and_replace_name_the_scratch(rocks: &Path, scratch: &Path) -> eyre::Result<()> {
        let scratch_name = scratch
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match RocksSubmoduleStore::open(rocks) {
            Ok(_) => {
                panic!("open created an index while close scratch {scratch_name} holds the bytes")
            }
            Err(err) => {
                let text = format!("{err:#}");
                assert!(
                    text.contains(&scratch_name),
                    "open must name the scratch: {text}"
                );
            }
        }
        if scratch.is_dir() {
            let err = RocksSubmoduleStore::open(scratch).unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains(&scratch_name),
                "open of the scratch must name it: {text}"
            );
        }
        let err = super::prepare_rocks_target(rocks, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&scratch_name),
            "replace must name the scratch: {text}"
        );
        let err = super::prepare_mdbx_target(rocks, testing_args()?, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&scratch_name),
            "mdbx replace must name the scratch: {text}"
        );
        Ok(())
    }

    /// A kill during a refused close must leave a sibling an operator can
    /// find. Open and `--replace-incomplete` name that sibling and leave its
    /// bytes in place.
    fn assert_killed_refused_close(
        label: &str,
        stop: crate::submodule::rocks::CloseScratchStop,
    ) -> eyre::Result<()> {
        use crate::submodule::rocks::CloseScratchStop::{
            Close, EmptyCreate, Marker, Rename, Restore,
        };
        let (root, rocks, seal_err) = seal_with_lock_removed(label, Some(stop))?;
        assert!(
            seal_err.contains("stopped refused close"),
            "seal did not stop inside the refused close: {seal_err}"
        );
        let marker = root.path().join(".rocks.close-scratch-marker");
        let Some(scratch) = close_scratch_location(root.path()) else {
            panic!("refused close left no marker and no scratch sibling");
        };
        let scratch_name = scratch
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match stop {
            Marker => {
                assert!(
                    marker.is_file(),
                    "scratch {scratch_name} is not named by .rocks.close-scratch-marker"
                );
                assert!(
                    !scratch.exists(),
                    "scratch must not exist before the rename"
                );
                assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
            }
            Rename | EmptyCreate | Close => {
                assert_eq!(fs::read(scratch.join("KEEP"))?, b"keep");
            }
            Restore => {
                assert!(
                    marker.is_file(),
                    "scratch {scratch_name} is not named by .rocks.close-scratch-marker"
                );
                assert!(!scratch.exists(), "scratch was renamed back");
                assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
            }
        }
        assert_open_and_replace_name_the_scratch(&rocks, &scratch)?;
        assert!(
            marker.is_file(),
            "scratch {scratch_name} is not named by .rocks.close-scratch-marker"
        );
        match stop {
            Marker | Restore => {
                assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
            }
            Rename => {
                assert!(
                    !rocks.exists(),
                    "replace must not create the missing directory"
                );
                assert_eq!(fs::read(scratch.join("KEEP"))?, b"keep");
            }
            EmptyCreate | Close => {
                assert_eq!(fs::read(scratch.join("KEEP"))?, b"keep");
                assert!(!rocks.join("SCHEMA").is_file());
            }
        }
        if matches!(stop, Marker | Restore) {
            let err = super::replace_marked_incomplete(&rocks, true, true).unwrap_err();
            assert!(format!("{err:#}").contains(&scratch_name), "{err:#}");
            assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        }
        Ok(())
    }

    #[test]
    fn killed_close_after_the_marker_names_the_scratch() -> eyre::Result<()> {
        assert_killed_refused_close(
            "convert-close-after-marker",
            crate::submodule::rocks::CloseScratchStop::Marker,
        )
    }

    #[test]
    fn killed_close_after_the_rename_names_the_scratch() -> eyre::Result<()> {
        assert_killed_refused_close(
            "convert-close-after-rename",
            crate::submodule::rocks::CloseScratchStop::Rename,
        )
    }

    #[test]
    fn killed_close_after_the_empty_directory_names_the_scratch() -> eyre::Result<()> {
        assert_killed_refused_close(
            "convert-close-after-empty",
            crate::submodule::rocks::CloseScratchStop::EmptyCreate,
        )
    }

    #[test]
    fn killed_close_after_the_flush_names_the_scratch() -> eyre::Result<()> {
        assert_killed_refused_close(
            "convert-close-after-flush",
            crate::submodule::rocks::CloseScratchStop::Close,
        )
    }

    #[test]
    fn killed_close_after_restore_keeps_the_marker() -> eyre::Result<()> {
        assert_killed_refused_close(
            "convert-close-after-restore",
            crate::submodule::rocks::CloseScratchStop::Restore,
        )
    }

    /// The path is already gone. Close must not create it and publish `CURRENT`.
    #[test]
    fn refused_close_of_a_missing_directory_does_not_publish() -> eyre::Result<()> {
        let root = temp("convert-close-missing");
        let rocks = root.path().join("rocks");
        let moved = root.path().join("rocks-moved");
        super::prepare_rocks_target(&rocks, false)?;
        let opened = RocksSubmoduleStore::open_converting(&rocks)?;
        fs::rename(&rocks, &moved)?;
        fs::write(moved.join("KEEP"), b"keep")?;
        let err = opened.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("missing") && text.contains("Refusing to create"),
            "{text}"
        );
        assert!(
            !rocks.exists(),
            "close must not create a directory and publish into it"
        );
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        assert_eq!(fs::read(moved.join("KEEP"))?, b"keep");
        Ok(())
    }

    /// A refused close that runs to the end puts the directory back and
    /// removes the scratch marker.
    #[test]
    fn completed_refused_close_restores_the_directory() -> eyre::Result<()> {
        let (root, rocks, text) = seal_with_lock_removed("convert-close-complete", None)?;
        assert!(text.contains("replaced"), "{text}");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!rocks.join("SCHEMA").is_file());
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        let left = fs::read_dir(root.path())?.any(|entry| {
            entry.ok().is_some_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("close-scratch-")
            })
        });
        assert!(!left, "completed close must not leave a scratch sibling");
        Ok(())
    }

    /// A normal seal writes SCHEMA and does not park the directory.
    #[test]
    fn successful_seal_leaves_no_close_scratch() -> eyre::Result<()> {
        let root = temp("convert-seal-clean");
        let rocks = root.path().join("rocks");
        super::prepare_rocks_target(&rocks, false)?;
        let opened = RocksSubmoduleStore::open_converting(&rocks)?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        opened.seal_and_stamp()?;
        assert!(rocks.join("SCHEMA").is_file());
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        Ok(())
    }

    /// The directory inode was replaced after this database opened. Seal must
    /// not write SCHEMA into the new directory or delete its log. `LOCK` on
    /// the new directory is a different file, so a flock of that path is not
    /// this database's lock.
    #[test]
    fn seal_does_not_publish_schema_into_a_replaced_directory() -> eyre::Result<()> {
        let root = temp("convert-seal-replaced");
        let rocks = root.path().join("rocks");
        let moved = root.path().join("rocks-moved");
        super::prepare_rocks_target(&rocks, false)?;
        let opened = RocksSubmoduleStore::open_converting(&rocks)?;
        fs::rename(&rocks, &moved)?;
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000099\n")?;
        fs::write(rocks.join("000099.log"), b"replacement-log")?;
        fs::write(rocks.join("LOCK"), b"")?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let err = opened.seal_and_stamp().unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("replaced"),
            "seal must refuse the new directory, not fail earlier: {text}"
        );
        assert!(
            !rocks.join("SCHEMA").is_file(),
            "replaced directory must not gain SCHEMA"
        );
        assert_eq!(fs::read(rocks.join("000099.log"))?, b"replacement-log");
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000099\n");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(!moved.join("SCHEMA").is_file());
        assert!(!root.path().join(".rocks.close-scratch-marker").exists());
        let left = fs::read_dir(root.path())?.any(|entry| {
            entry.ok().is_some_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("close-scratch-")
            })
        });
        assert!(!left, "completed close must not leave a scratch sibling");
        Ok(())
    }

    /// `CONVERTING` is written only after `LOCK` is flocked. A replace that
    /// runs in that write must see the flock. Looking after both calls return
    /// stays green if the sentinel is written first.
    fn assert_replace_during_sentinel_write(
        label: &str,
        prepare: impl Fn(&Path, bool) -> eyre::Result<std::sync::Arc<std::fs::File>> + 'static,
    ) -> eyre::Result<()> {
        let root = temp(label);
        let path = root.path().join("index");
        fs::create_dir_all(&path)?;
        let prepare = std::sync::Arc::new(prepare);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let flag = std::sync::Arc::clone(&seen);
        let inner = std::sync::Arc::clone(&prepare);
        let _hook = super::set_after_converting_written(move |dir| {
            let _ = fs::write(dir.join("KEEP"), b"keep");
            let text = match inner(dir, true) {
                Ok(_) => "ok".to_owned(),
                Err(err) => format!("{err:#}"),
            };
            let kept = dir.join("KEEP").is_file();
            *flag.lock().expect("sentinel window") = Some((text, kept));
        });
        let held = prepare(&path, false);
        let (text, kept) = seen.lock().expect("sentinel window").clone().expect("hook");
        assert!(
            text.contains("rocksdb LOCK"),
            "replace must see the flock, not a directory that has no LOCK yet: {text}"
        );
        assert!(kept, "replace deleted the directory before the flock");
        assert_eq!(fs::read(path.join("KEEP"))?, b"keep");
        let held = held?;
        assert!(path.join("CONVERTING").is_file());
        assert!(path.join("LOCK").is_file());
        drop(held);
        drop(_hook);
        Ok(())
    }

    #[test]
    fn replace_during_sentinel_write_sees_the_rocks_lock() -> eyre::Result<()> {
        assert_replace_during_sentinel_write("convert-sentinel-after-lock", |path, replace| {
            super::prepare_rocks_target(path, replace)
        })
    }

    #[test]
    fn replace_during_sentinel_write_sees_the_mdbx_lock() -> eyre::Result<()> {
        let args = testing_args()?;
        assert_replace_during_sentinel_write(
            "convert-mdbx-sentinel-after-lock",
            move |path, replace| super::prepare_mdbx_target(path, args.clone(), replace),
        )
    }

    /// `CONVERTING` is written only after `LOCK` is flocked. A replace that
    /// sees the sentinel must fail while that flock is held, and must succeed
    /// after the holder drops it.
    #[test]
    fn held_conversion_lock_blocks_replace_before_the_database_opens() -> eyre::Result<()> {
        let root = temp("convert-lock-before-open");
        let rocks = root.path().join("rocks");
        let held = super::prepare_rocks_target(&rocks, false)?;
        assert!(rocks.join("LOCK").is_file());
        assert!(rocks.join("CONVERTING").is_file());
        assert!(!rocks.join("CURRENT").exists());
        fs::write(rocks.join("KEEP"), b"keep")?;
        let err = super::prepare_rocks_target(&rocks, true).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(rocks.join("CONVERTING").is_file());
        drop(held);
        super::prepare_rocks_target(&rocks, true)?;
        assert!(!rocks.join("KEEP").is_file());
        assert!(rocks.join("CONVERTING").is_file());
        assert!(!rocks.join("SCHEMA").is_file());
        Ok(())
    }

    /// Same order for an MDBX target: the flock is held before `mdbx.dat` exists.
    #[test]
    fn held_conversion_lock_blocks_mdbx_replace_before_the_env_opens() -> eyre::Result<()> {
        let root = temp("convert-mdbx-lock-before-open");
        let path = root.path().join("mdbx");
        let held = super::prepare_mdbx_target(&path, testing_args()?, false)?;
        assert!(path.join("LOCK").is_file());
        assert!(path.join("CONVERTING").is_file());
        assert!(!path.join("mdbx.dat").exists());
        fs::write(path.join("KEEP"), b"keep")?;
        let err = super::prepare_mdbx_target(&path, testing_args()?, true).unwrap_err();
        assert!(format!("{err:#}").contains("may be running"), "{err:#}");
        assert_eq!(fs::read(path.join("KEEP"))?, b"keep");
        drop(held);
        super::prepare_mdbx_target(&path, testing_args()?, true)?;
        assert!(!path.join("KEEP").is_file());
        assert!(path.join("CONVERTING").is_file());
        assert!(!path.join("mdbx.dat").exists());
        Ok(())
    }

    /// `CURRENT` with no `LOCK` file is not a dropped conversion. Delete would
    /// remove a database that this process cannot prove is idle.
    #[test]
    fn replace_incomplete_refuses_current_without_a_lock_file() -> eyre::Result<()> {
        let root = temp("convert-current-without-lock");
        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000001\n")?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let err = super::prepare_rocks_target(&rocks, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("CURRENT") && text.contains("LOCK"), "{text}");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000001\n");
        assert!(rocks.join("CONVERTING").is_file());
        Ok(())
    }

    /// A replace that runs after the database closes and before SCHEMA exists
    /// must not delete the directory being sealed.
    #[test]
    fn replace_incomplete_does_not_delete_during_schema_write() -> eyre::Result<()> {
        let root = temp("convert-replace-sealing");
        let rocks = root.path().join("rocks");
        super::prepare_rocks_target(&rocks, false)?;
        let mut opened = RocksSubmoduleStore::open_converting(&rocks)?;
        fs::write(rocks.join("KEEP"), b"keep")?;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let path = rocks.clone();
        let flag = std::sync::Arc::clone(&seen);
        opened.set_before_schema_hook(move || {
            let text = match super::prepare_rocks_target(&path, true) {
                Ok(_) => "ok".to_owned(),
                Err(err) => format!("{err:#}"),
            };
            *flag.lock().expect("seal window") = Some((
                text,
                path.join("CONVERTING").is_file(),
                path.join("SCHEMA").is_file(),
                path.join("KEEP").is_file(),
            ));
        });
        opened.seal_and_stamp()?;
        let (text, converting, schema, kept) =
            seen.lock().expect("seal window").clone().expect("hook");
        assert!(text.contains("may be running"), "{text}");
        assert!(converting);
        assert!(!schema);
        assert!(kept);
        assert!(rocks.join("SCHEMA").is_file());
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        let err = super::prepare_rocks_target(&rocks, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");
        assert_eq!(fs::read(rocks.join("KEEP"))?, b"keep");
        assert!(rocks.join("SCHEMA").is_file());
        Ok(())
    }

    /// `..` through a missing directory is the source or the storage module
    /// after the kernel resolves it. The missing hop must not be created.
    /// A path that collapses inside the parent is still a valid new target.
    #[test]
    fn dotted_target_does_not_write_through_a_missing_component() -> eyre::Result<()> {
        let root = temp("convert-dotted-target");
        let module = root.path().join("module");
        let db = module.join("db");
        fs::create_dir_all(&db)?;
        fs::write(db.join("mdbx.dat"), b"index")?;

        let into_source = module.join("other").join("..").join("db");
        let err = super::prepare_rocks_target(&into_source, false).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("mdbx.dat") || text.contains("same path"),
            "{text}"
        );
        assert!(
            !module.join("other").exists(),
            "missing component must not be created"
        );
        assert!(!db.join("CONVERTING").is_file());

        fs::write(module.join("chunks.dat"), b"chunks")?;
        let into_module = module.join("db").join("ghost").join("..").join("..");
        let err = super::prepare_rocks_target(&into_module, false).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("chunks.dat") || text.contains("overlap") || text.contains("same path"),
            "{text}"
        );
        assert!(!module.join("db").join("ghost").exists());
        assert!(!module.join("CONVERTING").is_file());

        let collapsed = module.join("sub").join("..").join("rocks");
        super::prepare_rocks_target(&collapsed, false)?;
        assert!(module.join("rocks").join("CONVERTING").is_file());
        assert!(!module.join("sub").exists());
        assert!(!db.join("CONVERTING").is_file());

        let options = super::ConvertOptions::new(testing_args()?);
        let err = convert_mdbx_to_rocks(&db, &into_source, &options).unwrap_err();
        assert!(format!("{err:#}").contains("same path"), "{err:#}");
        assert!(!module.join("other").exists());
        Ok(())
    }

    /// A path that collapses off its own start is refused, and the error
    /// names that collapsed form. A path that stays inside its start is kept.
    #[test]
    fn unanchored_target_names_the_collapsed_form() {
        for path in [".", "../rocks", "db/../.."] {
            let err = super::lexical_target(Path::new(path)).unwrap_err();
            let text = err.to_string();
            assert!(text.contains("not anchored"), "{path}: {text}");
            assert!(
                text.contains("collapsed form") && text.contains("\"\""),
                "{path}: {text}"
            );
        }
        assert_eq!(
            super::lexical_target(Path::new("sub/../rocks")).unwrap(),
            PathBuf::from("rocks")
        );
        assert_eq!(
            super::lexical_target(Path::new("/tmp/rocks")).unwrap(),
            PathBuf::from("/tmp/rocks")
        );
    }

    /// A relative target is resolved from the current directory. When that
    /// directory is the source, the target sits inside it and must be refused.
    /// A source that is not an ancestor of the current directory stays usable.
    #[test]
    fn relative_target_inside_the_source_is_refused() -> eyre::Result<()> {
        let source = std::env::current_dir()?;
        let options = super::ConvertOptions::new(testing_args()?);
        let inside = Path::new("sweep16-x4-relative-inside-source");
        assert!(
            !source.join(inside).exists(),
            "the relative name must not already exist"
        );
        let err = super::guard_paths(&source, inside, &options).unwrap_err();
        assert!(format!("{err:#}").contains("overlap"), "{err:#}");

        let root = temp("convert-relative-outside");
        let outside = root.path().join("source");
        fs::create_dir(&outside)?;
        super::guard_paths(
            &outside,
            Path::new("sweep16-x4-relative-outside-source"),
            &options,
        )?;
        Ok(())
    }

    /// Incomplete MDBX directory: convert schema, `CONVERTING`, and a file the
    /// delete must remove only when the replace is allowed to finish.
    fn incomplete_mdbx(root: &Path, name: &str) -> eyre::Result<PathBuf> {
        let path = root.join(name);
        fs::create_dir_all(&path)?;
        drop(MdbxSubmoduleStore::open_for_convert(
            &path,
            testing_args()?.with_exclusive(Some(true)),
        )?);
        fs::write(path.join("CONVERTING"), super::SUBMODULE_CONVERT_INCOMPLETE)?;
        fs::write(path.join("KEEP"), b"keep")?;
        Ok(path)
    }

    /// `mdbx.dat` can appear after the first stat and before `remove_dir_all`.
    /// The delete must not run. The exclusive open sees the completed schema.
    #[test]
    fn replace_refuses_when_mdbx_dat_appears_before_the_delete() -> eyre::Result<()> {
        let root = temp("convert-mdbx-appears");
        let path = root.path().join("mdbx");
        fs::create_dir_all(&path)?;
        fs::write(path.join("CONVERTING"), super::SUBMODULE_CONVERT_INCOMPLETE)?;
        fs::write(path.join("KEEP"), b"keep")?;
        let _guard = super::set_before_replace_delete(|dir| {
            let store = MdbxSubmoduleStore::open_for_convert(
                dir,
                testing_args().expect("args").with_exclusive(Some(true)),
            )
            .expect("plant mdbx");
            store
                .put_schema_bytes(super::SUBMODULE_SCHEMA.as_bytes())
                .expect("completed schema");
        });
        let err = super::prepare_mdbx_target(&path, testing_args()?, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("completed"), "{text}");
        assert_eq!(fs::read(path.join("KEEP"))?, b"keep");
        assert!(path.join("mdbx.dat").is_file());
        assert!(path.join("CONVERTING").is_file());
        let store = MdbxSubmoduleStore::open(&path, testing_args()?)?;
        assert_eq!(
            store.schema_bytes()?.as_deref(),
            Some(super::SUBMODULE_SCHEMA.as_bytes())
        );
        Ok(())
    }

    /// Clearing the sentinel drops the flock file on an MDBX directory.
    /// A RocksDB directory has `CURRENT` and keeps `LOCK`.
    #[test]
    fn clear_converting_sentinel_drops_an_mdbx_lock_and_keeps_a_rocks_lock() -> eyre::Result<()> {
        let root = temp("convert-clear-lock");
        let mdbx = root.path().join("mdbx");
        fs::create_dir_all(&mdbx)?;
        fs::write(mdbx.join("CONVERTING"), super::SUBMODULE_CONVERT_INCOMPLETE)?;
        fs::write(mdbx.join("LOCK"), b"")?;
        fs::write(mdbx.join("SCHEMA"), super::SUBMODULE_SCHEMA)?;
        super::clear_converting_sentinel(&mdbx)?;
        assert!(!mdbx.join("CONVERTING").exists());
        assert!(!mdbx.join("LOCK").exists());
        assert!(mdbx.join("SCHEMA").is_file());

        let rocks = root.path().join("rocks");
        fs::create_dir_all(&rocks)?;
        fs::write(
            rocks.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(rocks.join("LOCK"), b"held")?;
        fs::write(rocks.join("CURRENT"), b"MANIFEST-000001\n")?;
        super::clear_converting_sentinel(&rocks)?;
        assert!(!rocks.join("CONVERTING").exists());
        assert_eq!(fs::read(rocks.join("LOCK"))?, b"held");
        assert_eq!(fs::read(rocks.join("CURRENT"))?, b"MANIFEST-000001\n");
        Ok(())
    }

    /// `--replace-incomplete` deletes an unfinished MDBX target and starts again.
    #[test]
    fn replace_incomplete_deletes_an_incomplete_mdbx_directory() -> eyre::Result<()> {
        let root = temp("convert-mdbx-replace-incomplete");
        let path = incomplete_mdbx(root.path(), "mdbx")?;
        super::prepare_mdbx_target(&path, testing_args()?, true)?;
        assert!(!path.join("KEEP").is_file());
        assert!(!path.join("mdbx.dat").is_file());
        assert!(!path.join("lock").is_file());
        assert_eq!(
            fs::read(path.join("CONVERTING"))?,
            super::SUBMODULE_CONVERT_INCOMPLETE.as_bytes()
        );
        Ok(())
    }

    /// A finished schema row is not a partial conversion, even with `CONVERTING`.
    #[test]
    fn prepare_mdbx_target_refuses_a_completed_mdbx_schema() -> eyre::Result<()> {
        let root = temp("convert-mdbx-completed");
        let path = incomplete_mdbx(root.path(), "mdbx")?;
        {
            let store = MdbxSubmoduleStore::open_for_convert(
                &path,
                testing_args()?.with_exclusive(Some(true)),
            )?;
            store.put_schema_bytes(super::SUBMODULE_SCHEMA.as_bytes())?;
        }
        let before = fs::metadata(path.join("mdbx.dat"))?.len();
        let err = super::prepare_mdbx_target(&path, testing_args()?, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");
        assert_eq!(fs::read(path.join("KEEP"))?, b"keep");
        assert!(path.join("mdbx.dat").is_file());
        assert_eq!(fs::metadata(path.join("mdbx.dat"))?.len(), before);
        assert!(!path.join("lock").is_file());
        assert!(path.join("CONVERTING").is_file());
        Ok(())
    }

    /// A schema committed after the first read and before the delete is current.
    /// The delete must leave that directory in place.
    #[test]
    fn replace_incomplete_keeps_an_mdbx_schema_committed_during_the_check_gap() -> eyre::Result<()>
    {
        let root = temp("convert-mdbx-schema-gap");
        let path = incomplete_mdbx(root.path(), "mdbx")?;
        let _guard = super::set_during_mdbx_replace_env(|env| {
            let bytes = super::SUBMODULE_SCHEMA.as_bytes().to_vec();
            env.update_eyre(|tx| crate::submodule::db::write_schema_bytes(tx, &bytes))
                .expect("commit schema");
        });
        let err = super::prepare_mdbx_target(&path, testing_args()?, true).unwrap_err();
        assert!(format!("{err:#}").contains("completed"), "{err:#}");
        assert_eq!(fs::read(path.join("KEEP"))?, b"keep");
        assert!(path.join("mdbx.dat").is_file());
        let store = MdbxSubmoduleStore::open(&path, testing_args()?)?;
        assert_eq!(
            store.schema_bytes()?.as_deref(),
            Some(super::SUBMODULE_SCHEMA.as_bytes())
        );
        assert!(path.join("CONVERTING").is_file());
        Ok(())
    }

    /// The environment stays open through the delete. Another process cannot
    /// seal in that window. The child must fail to open, and the incomplete
    /// directory is still replaced. A child that commits `SUBMODULE_SCHEMA`
    /// is not a passing outcome: the schema re-read already covers that.
    #[test]
    fn replace_incomplete_blocks_another_process_until_the_mdbx_directory_is_removed()
    -> eyre::Result<()> {
        if std::env::var_os("IRYS_MDBX_REPLACE_CHILD").is_some() {
            return seal_incomplete_mdbx_child();
        }
        let root = temp("convert-mdbx-replace-race");
        let path = incomplete_mdbx(root.path(), "mdbx")?;
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(None));
        let flag = std::sync::Arc::clone(&recorded);
        let _guard = super::set_during_mdbx_incomplete_replace(move |dir| {
            let output = std::process::Command::new(std::env::current_exe().expect("test bin"))
                .arg(
                    "replace_incomplete_blocks_another_process_until_the_mdbx_directory_is_removed",
                )
                .env("IRYS_MDBX_REPLACE_CHILD", "1")
                .env("IRYS_MDBX_REPLACE_DIR", dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("spawn sealer");
            *flag.lock().expect("sealer output") = Some(output);
        });
        let prepared = super::prepare_mdbx_target(&path, testing_args()?, true);
        let output = recorded
            .lock()
            .expect("sealer output")
            .take()
            .expect("hook ran");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let lower = text.to_ascii_lowercase();
        assert!(
            !output.status.success(),
            "sealer opened the index while replace held it: {text}"
        );
        assert!(
            lower.contains("busy")
                || lower.contains("another write transaction")
                || lower.contains("in use")
                || lower.contains("exclusive")
                || lower.contains("lock"),
            "code={:?} text={text}",
            output.status.code()
        );
        prepared?;
        assert!(!path.join("KEEP").is_file());
        assert!(!path.join("mdbx.dat").is_file());
        assert!(!path.join("lock").is_file());
        assert!(path.join("CONVERTING").is_file());
        Ok(())
    }

    fn seal_incomplete_mdbx_child() -> eyre::Result<()> {
        let path = PathBuf::from(std::env::var("IRYS_MDBX_REPLACE_DIR")?);
        let args = testing_args()?.with_exclusive(Some(true));
        let store = MdbxSubmoduleStore::open_for_convert(&path, args)
            .map_err(|err| eyre::eyre!("seal open failed: {err}"))?;
        store.put_schema_bytes(super::SUBMODULE_SCHEMA.as_bytes())?;
        Ok(())
    }

    /// A finished RocksDB index can still hold `CONVERTING` when the process
    /// dies after `SCHEMA` and before the sentinel is removed. Replacing an
    /// MDBX target must leave that directory in place.
    #[test]
    fn prepare_mdbx_target_refuses_a_finished_rocks_schema() -> eyre::Result<()> {
        let root = temp("convert-mdbx-target-schema");
        let finished = root.path().join("finished-rocks");
        fs::create_dir_all(&finished)?;
        fs::write(finished.join("SCHEMA"), SUBMODULE_SCHEMA_ROCKS_V1)?;
        fs::write(finished.join("CURRENT"), b"MANIFEST-000001\n")?;
        fs::write(
            finished.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(finished.join("KEEP"), b"keep")?;
        let err = super::prepare_mdbx_target(&finished, testing_args()?, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("completed RocksDB index"), "{text}");
        assert_eq!(
            fs::read(finished.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()
        );
        assert_eq!(fs::read(finished.join("CURRENT"))?, b"MANIFEST-000001\n");
        assert_eq!(fs::read(finished.join("KEEP"))?, b"keep");
        assert_eq!(
            fs::read(finished.join("CONVERTING"))?,
            super::SUBMODULE_CONVERT_INCOMPLETE.as_bytes()
        );

        let foreign = root.path().join("foreign-schema");
        fs::create_dir_all(&foreign)?;
        fs::write(foreign.join("SCHEMA"), b"not-a-submodule\n")?;
        fs::write(
            foreign.join("CONVERTING"),
            super::SUBMODULE_CONVERT_INCOMPLETE,
        )?;
        fs::write(foreign.join("KEEP"), b"keep")?;
        let err = super::prepare_mdbx_target(&foreign, testing_args()?, true).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("existing index schema"), "{text}");
        assert_eq!(fs::read(foreign.join("SCHEMA"))?, b"not-a-submodule\n");
        assert_eq!(fs::read(foreign.join("KEEP"))?, b"keep");
        Ok(())
    }

    #[test]
    fn dry_run_matches_a_later_convert_and_writes_nothing() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-dry")?;
        let mut dry = options.clone();
        dry.dry_run = true;
        let preview = convert_mdbx_to_rocks(&mdbx, &rocks, &dry)?;
        assert!(!rocks.exists());
        assert_copy_counts(&preview);
        let written = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(preview.intervals, written.intervals);
        assert_eq!(preview.data_paths, written.data_paths);
        assert_eq!(preview.pending, written.pending);
        assert_eq!(preview.tx_leaf_bindings, written.tx_leaf_bindings);
        assert!(rocks.join("SCHEMA").is_file());
        Ok(())
    }

    /// `--verify` keeps one source open. The RocksDB bytes match a plain convert.
    #[test]
    fn mdbx_to_rocks_verify_matches_a_plain_convert() -> eyre::Result<()> {
        let Populated {
            root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-verify-same-bytes")?;
        let plain = root.path().join("rocks-plain");
        convert_mdbx_to_rocks(&mdbx, &plain, &options)?;
        let (stats, report) = convert_mdbx_to_rocks_and_verify(&mdbx, &rocks, &options)?;
        assert_copy_counts(&stats);
        assert_equivalent(&report);
        let same = verify_rocks_records(&plain, &rocks)?;
        assert!(!same.has_mismatch(), "{same}");
        Ok(())
    }

    #[test]
    fn reset_marker_writes_an_empty_index() -> eyre::Result<()> {
        let Populated {
            root,
            consensus,
            mdbx,
            rocks,
            options,
        } = populated("convert-reset")?;
        let back = root.path().join("db-back");
        {
            let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
            store.set_reset_in_progress()?;
        }
        let mut dry = options.clone();
        dry.dry_run = true;
        let preview = convert_mdbx_to_rocks(&mdbx, &rocks, &dry)?;
        assert!(!rocks.exists());
        assert!(preview.reset_marker);
        assert_eq!(preview.intervals, 0);
        assert_eq!(preview.data_paths, 0);

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert!(stats.reset_marker);
        assert_eq!(stats.intervals, 0);
        assert_eq!(stats.data_paths, 0);
        assert_eq!(stats.pending, 0);
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            assert!(opened.reset_in_progress()?);
            opened.view(|tx| {
                assert!(tx.placement_at(PartitionChunkOffset(0))?.is_none());
                assert!(
                    tx.get_data_path_by_offset(PartitionChunkOffset(0))?
                        .is_none()
                );
                Ok(())
            })?;
        }
        assert_v2(
            &MdbxSubmoduleStore::open(&mdbx, testing_args()?)?,
            &consensus,
        )?;
        assert!(MdbxSubmoduleStore::open(&mdbx, testing_args()?)?.reset_in_progress()?);

        let back_stats = convert_rocks_to_mdbx(&rocks, &back, &options)?;
        assert!(back_stats.reset_marker);
        assert_eq!(back_stats.intervals, 0);
        let restored = MdbxSubmoduleStore::open(&back, testing_args()?)?;
        assert!(restored.reset_in_progress()?);
        restored.view(|tx| {
            assert!(tx.get_tx_path_by_offset(PartitionChunkOffset(0))?.is_none());
            Ok(())
        })?;
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        Ok(())
    }

    fn add_orphan_and_frontier(mdbx: &Path) -> eyre::Result<()> {
        let store = MdbxSubmoduleStore::open(mdbx, testing_args()?)?;
        let hash = hid(77);
        store.update(|tx| {
            tx.add_full_data_path(PartitionChunkOffset(7), b"orphan-path".to_vec())?;
            tx.add_full_tx_path(hash, b"frontier-path".to_vec())?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(20),
                PartitionChunkOffset(20),
                Some(hash),
            )?;
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn unresolved_placements_fail_unless_allowed() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-skip")?;
        add_orphan_and_frontier(&mdbx)?;

        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("does not cover"), "{text}");
        assert!(text.contains("frontier is 13"), "{text}");
        assert!(
            !rocks.exists(),
            "a behind consensus database must not create the target"
        );

        let mut dry = options.clone();
        dry.dry_run = true;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &dry).unwrap_err();
        assert!(format!("{err:#}").contains("does not cover"), "{err:#}");
        assert!(!rocks.exists());

        let mut allowed = options.clone();
        allowed.allow_unresolved = true;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &allowed)?;
        assert!(stats.skipped_orphans >= 1, "{stats}");
        assert!(stats.skipped_frontier >= 1, "{stats}");
        assert_eq!(stats.intervals, 2);
        assert!(rocks.join("SCHEMA").is_file());
        // Drop the writer before the read-only verify. The writer can delete
        // an SST that the on-disk manifest still names, and the second open fails.
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.view(|tx| {
                assert!(
                    tx.get_data_path_by_offset(PartitionChunkOffset(7))?
                        .is_none()
                );
                assert!(tx.placement_at(PartitionChunkOffset(20))?.is_none());
                Ok(())
            })?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.skipped_orphans >= 1, "{report}");
        assert!(report.skipped_frontier >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn empty_consensus_block_index_is_refused() -> eyre::Result<()> {
        let Populated {
            root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-empty-consensus")?;
        let empty = root.path().join("empty-consensus");
        fs::create_dir_all(&empty)?;
        let db = open_or_create_db(&empty, IrysTables::ALL, testing_args()?)?;
        db.update_eyre(|tx| {
            crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
            Ok(())
        })?;
        drop(db);

        let mut allowed = options_for(&empty)?;
        allowed.allow_unresolved = true;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &allowed).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("block index is empty"), "{text}");
        assert!(text.contains("even with --allow-unresolved"), "{text}");
        assert!(!rocks.exists());

        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let back = root.path().join("db-back");
        let err = convert_rocks_to_mdbx(&rocks, &back, &allowed).unwrap_err();
        assert!(
            format!("{err:#}").contains("block index is empty"),
            "{err:#}"
        );
        assert!(!back.exists());
        Ok(())
    }

    #[test]
    fn verify_counts_a_missing_target_placement() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-missing-place")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!clean.has_mismatch(), "{clean}");
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset(0),
                    PartitionChunkOffset(2),
                    None,
                )?;
                Ok(())
            })?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.intervals_mismatches >= 1, "{report}");
        assert!(report.has_mismatch(), "{report}");
        Ok(())
    }

    #[test]
    fn merged_identical_roots_split_into_one_placement_per_tx() -> eyre::Result<()> {
        let root = temp("convert-split-same-root");
        let consensus = root.path().join("consensus");
        fs::create_dir_all(&consensus)?;
        let mut left = tx_at(1, 32);
        let mut right = tx_at(2, 32);
        let data_root = hid(50);
        let prefix = hid(60);
        left.data_root = data_root;
        right.data_root = data_root;
        left.prefix_hash = prefix;
        right.prefix_hash = prefix;
        let blocks = [
            block_with(0, &[], 0),
            block_with(1, &[left.clone(), right.clone()], 2),
        ];
        let db = open_or_create_db(&consensus, IrysTables::ALL, testing_args()?)?;
        db.update_eyre(|tx| {
            insert_tx_header(tx, &left)?;
            insert_tx_header(tx, &right)?;
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
            Ok(())
        })?;
        drop(db);

        let opened = open_consensus(&consensus)?;
        let first = derive_tx_path_at_offset(&opened, DataLedger::Publish, 0, 32)?;
        let second = derive_tx_path_at_offset(&opened, DataLedger::Publish, 1, 32)?;
        assert_ne!(first.tx_index, second.tx_index);
        assert_eq!(first.data_root, data_root);
        let stored_hash = tx_path_hash(&first.proof.proof);
        drop(opened);

        let mdbx = root.path().join("db");
        let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
        store.update(|tx| {
            tx.add_full_tx_path(stored_hash, first.proof.proof.clone())?;
            tx.add_tx_leaf_binding(
                stored_hash,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: prefix,
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(0),
                PartitionChunkOffset(1),
                Some(stored_hash),
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: 32,
                },
            )?;
            Ok(())
        })?;
        drop(store);

        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.intervals, 2, "{stats}");
        assert_eq!(stats.skipped_frontier, 0, "{stats}");
        assert!(!rocks.join("CONVERTING").exists());
        assert_eq!(
            fs::read_to_string(rocks.join("SCHEMA"))?,
            SUBMODULE_SCHEMA_ROCKS_V1
        );
        let opened = RocksSubmoduleStore::open(&rocks)?;
        opened.view(|tx| {
            let head = tx
                .placement_at(PartitionChunkOffset(0))?
                .expect("first placement");
            assert_eq!(head.start, PartitionChunkOffset(0));
            assert_eq!(head.end, PartitionChunkOffset(0));
            assert_eq!(head.tx_index, Some(0));
            assert_eq!(head.data_root, data_root);
            assert_eq!(head.data_size, 32);
            assert_eq!(head.tx_start_offset, RelativeChunkOffset(0));
            let tail = tx
                .placement_at(PartitionChunkOffset(1))?
                .expect("second placement");
            assert_eq!(tail.start, PartitionChunkOffset(1));
            assert_eq!(tail.end, PartitionChunkOffset(1));
            assert_eq!(tail.tx_index, Some(1));
            assert_eq!(tail.data_root, data_root);
            assert_eq!(tail.data_size, 32);
            assert_eq!(tail.tx_start_offset, RelativeChunkOffset(1));
            Ok(())
        })?;
        // Drop the writable handle before verify opens the same directory.
        // A live handle compacts and deletes SST files that verify has
        // already read from the manifest.
        drop(opened);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(report.index_audit_findings, 0, "{report}");
        Ok(())
    }

    #[test]
    fn merged_interval_that_derives_another_root_is_refused() -> eyre::Result<()> {
        let root = temp("convert-split-other-root");
        let consensus = root.path().join("consensus");
        write_chain(&consensus)?;
        let pair = derived_pair(&consensus)?;
        let hash_b = tx_path_hash(&pair.proof_b);
        let mdbx = root.path().join("db");
        let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
        store.update(|tx| {
            tx.add_full_tx_path(hash_b, pair.proof_b.clone())?;
            tx.add_tx_leaf_binding(
                hash_b,
                &TxLeafBinding {
                    data_root: pair.root_b,
                    prefix_hash: pair.prefix_b,
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(3),
                PartitionChunkOffset(4),
                Some(hash_b),
            )?;
            tx.add_data_root_info(
                pair.root_b,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(3),
                    data_size: 32,
                },
            )?;
            tx.add_data_root_info(
                pair.root_b,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(4),
                    data_size: 32,
                },
            )?;
            Ok(())
        })?;
        drop(store);

        let rocks = root.path().join("rocks");
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("derives a different data root"), "{text}");
        assert!(!rocks.join("SCHEMA").exists());
        Ok(())
    }

    /// Consensus chain plus an open schema-v1 index. One empty block, then
    /// one block that holds `txs` and covers `total` chunks.
    fn open_v1_fixture(
        root: &Path,
        txs: &[DataTransactionHeader],
        total: u64,
    ) -> eyre::Result<(PathBuf, DatabaseEnv, DatabaseEnv)> {
        use crate::submodule::tables::v1_fixture::master;

        let consensus = root.join("consensus");
        fs::create_dir_all(&consensus)?;
        let blocks = [block_with(0, &[], 0), block_with(1, txs, total)];
        let db = open_or_create_db(&consensus, IrysTables::ALL, testing_args()?)?;
        db.update_eyre(|tx| {
            for header in txs {
                insert_tx_header(tx, header)?;
            }
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
            Ok(())
        })?;
        drop(db);
        let opened = open_consensus(&consensus)?;
        let index = open_or_create_db(
            root.join("db"),
            master::MasterV1Tables::ALL,
            testing_args()?,
        )?;
        Ok((consensus, opened, index))
    }

    /// One v1 tx covers every chunk. Adjacent offsets share its tx-path hash,
    /// so the copy keeps one run. Each data path is 16 distinct bytes.
    fn write_v1_one_tx_partition(root: &Path, n: u64) -> eyre::Result<PathBuf> {
        write_v1_one_tx_paths(root, n, 16)
    }

    /// Same run as `write_v1_one_tx_partition`, with each data path `path_len` bytes.
    fn write_v1_one_tx_paths(root: &Path, n: u64, path_len: usize) -> eyre::Result<PathBuf> {
        use crate::submodule::tables::v1_fixture::master;
        use crate::tx_path_derive::{TxPathBlockCache, derive_tx_path_at_offset_cached};
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let txs = [tx_at(1, n.saturating_mul(32))];
        let (consensus, opened, index) = open_v1_fixture(root, &txs, n)?;
        let mut cache = TxPathBlockCache::new();
        {
            let tx = index.tx_mut()?;
            let mut data_root = None;
            for offset in 0..n {
                let derived = derive_tx_path_at_offset_cached(
                    &opened,
                    &mut cache,
                    DataLedger::Publish,
                    offset,
                    32,
                )?;
                let proof = derived.proof.proof;
                let path_hash = tx_path_hash(&proof);
                let data = v1_path_of(offset, path_len);
                let data_hash = H256::from(hash_sha256(&data));
                let key = PartitionChunkOffset(offset as u32);
                tx.put::<master::ChunkPathHashesByOffset>(
                    key,
                    ChunkPathHashes {
                        data_path_hash: Some(data_hash),
                        tx_path_hash: Some(path_hash),
                    },
                )?;
                tx.put::<master::ChunkDataPathByPathHash>(data_hash, data)?;
                tx.put::<master::TxPathByTxPathHash>(path_hash, proof)?;
                data_root = Some(derived.data_root);
            }
            let root = data_root.ok_or_else(|| eyre::eyre!("no v1 chunks"))?;
            tx.put::<master::DataRootInfosByDataRoot>(
                root,
                DataRootInfos(vec![DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: n.saturating_mul(32),
                }]),
            )?;
            tx.commit()?;
        }
        drop(index);
        drop(opened);
        Ok(consensus)
    }

    fn v1_path_bytes(offset: u64) -> Vec<u8> {
        v1_path_of(offset, 16)
    }

    fn v1_path_of(offset: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0_u8; len.max(1)];
        data[0] = u8::try_from(offset).unwrap_or(u8::MAX);
        let last = data.len().saturating_sub(1);
        data[last] = 0x5a;
        data
    }

    /// One v1 tx over `n` chunks. `hole` keeps the tx-path hash and stores no
    /// data path, so the placement still covers that offset.
    fn write_v1_one_tx_with_hole(root: &Path, n: u64, hole: u64) -> eyre::Result<PathBuf> {
        write_v1_one_tx_with_holes(root, n, &[hole])
    }

    /// Same run as [`write_v1_one_tx_with_hole`], with every offset in `holes`
    /// storing a tx-path hash and no data path.
    fn write_v1_one_tx_with_holes(root: &Path, n: u64, holes: &[u64]) -> eyre::Result<PathBuf> {
        use crate::submodule::tables::v1_fixture::master;
        use crate::tx_path_derive::{TxPathBlockCache, derive_tx_path_at_offset_cached};
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let txs = [tx_at(1, n.saturating_mul(32))];
        let (consensus, opened, index) = open_v1_fixture(root, &txs, n)?;
        let mut cache = TxPathBlockCache::new();
        {
            let tx = index.tx_mut()?;
            let mut data_root = None;
            for offset in 0..n {
                let derived = derive_tx_path_at_offset_cached(
                    &opened,
                    &mut cache,
                    DataLedger::Publish,
                    offset,
                    32,
                )?;
                let proof = derived.proof.proof;
                let path_hash = tx_path_hash(&proof);
                let key = PartitionChunkOffset(offset as u32);
                let data_path_hash = if holes.contains(&offset) {
                    None
                } else {
                    let data = v1_path_bytes(offset);
                    let data_hash = H256::from(hash_sha256(&data));
                    tx.put::<master::ChunkDataPathByPathHash>(data_hash, data)?;
                    Some(data_hash)
                };
                tx.put::<master::ChunkPathHashesByOffset>(
                    key,
                    ChunkPathHashes {
                        data_path_hash,
                        tx_path_hash: Some(path_hash),
                    },
                )?;
                tx.put::<master::TxPathByTxPathHash>(path_hash, proof)?;
                data_root = Some(derived.data_root);
            }
            let root = data_root.ok_or_else(|| eyre::eyre!("no v1 chunks"))?;
            tx.put::<master::DataRootInfosByDataRoot>(
                root,
                DataRootInfos(vec![DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: n.saturating_mul(32),
                }]),
            )?;
            tx.commit()?;
        }
        drop(index);
        drop(opened);
        Ok(consensus)
    }

    /// A path batch smaller than one data path still copies the run. Offsets
    /// after the cap is crossed are not read until the range copy. A batch
    /// that can hold the paths reads each one once.
    #[test]
    fn v1_oversized_path_batch_does_not_reread_the_rest_of_the_run() -> eyre::Result<()> {
        const N: u64 = 5;
        let root = temp("convert-v1-path-cap");
        let consensus = write_v1_one_tx_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        options.batch_rows = 8;
        options.batch_value_bytes = 8;
        options.dry_run = true;

        reset_v1_data_path_reads();
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.intervals, 1, "{stats}");
        assert_eq!(stats.data_paths, N, "{stats}");
        assert_eq!(stats.path_hashes, N, "{stats}");
        assert_eq!(stats.skipped_frontier, 0, "{stats}");
        // First path is kept. The next path crosses the cap and is dropped.
        // The other three are not point-read. The range copy reads each hash
        // page once, and that read is not a point read.
        assert_eq!(v1_data_path_reads(), 2);

        options.batch_value_bytes = 8 * 1024 * 1024;
        options.dry_run = false;
        reset_v1_data_path_reads();
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(
            v1_data_path_reads(),
            usize::try_from(N).unwrap_or(usize::MAX)
        );
        assert_eq!(stats.intervals, 1, "{stats}");
        assert_eq!(stats.data_paths, N, "{stats}");
        assert_eq!(stats.skipped_frontier, 0, "{stats}");
        let opened = RocksSubmoduleStore::open(&rocks)?;
        opened.view(|tx| {
            assert_eq!(
                tx.get_data_path_by_offset(PartitionChunkOffset(4))?,
                Some(v1_path_bytes(4))
            );
            assert!(tx.placement_at(PartitionChunkOffset(0))?.is_some());
            assert!(tx.placement_at(PartitionChunkOffset(4))?.is_some());
            Ok(())
        })?;
        Ok(())
    }

    struct V1CapRun {
        _root: tempfile::TempDir,
        rocks: PathBuf,
        stats: ConvertStats,
        batches: Vec<usize>,
        reads: usize,
        /// Copy-rescan views. Point reads stay in `reads`.
        copy_views: usize,
    }

    /// One v1 run copied under a path-byte cap. `batches` is the path bytes
    /// in each range-copy write.
    fn run_v1_path_cap(
        prefix: &str,
        n: u64,
        path_len: usize,
        value_bytes: usize,
    ) -> eyre::Result<V1CapRun> {
        run_v1_path_cap_grouped(prefix, n, path_len, value_bytes, None)
    }

    /// `group` splits each hash page. `None` is the production page size.
    fn run_v1_path_cap_grouped(
        prefix: &str,
        n: u64,
        path_len: usize,
        value_bytes: usize,
        group: Option<usize>,
    ) -> eyre::Result<V1CapRun> {
        let root = temp(prefix);
        let consensus = write_v1_one_tx_paths(root.path(), n, path_len)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = n;
        options.batch_rows = 16;
        options.batch_value_bytes = value_bytes;
        let _group = group.map(set_v1_path_view_group);
        reset_v1_data_path_reads();
        reset_v1_range_copy_batches();
        reset_v1_copy_path_views();
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        Ok(V1CapRun {
            _root: root,
            rocks,
            stats,
            batches: v1_range_copy_batches(),
            reads: v1_data_path_reads(),
            copy_views: v1_copy_path_views(),
        })
    }

    fn assert_v1_paths_stored(rocks: &Path, n: u64, path_len: usize) -> eyre::Result<()> {
        let opened = RocksSubmoduleStore::open(rocks)?;
        opened.view(|tx| {
            for offset in 0..n {
                assert_eq!(
                    tx.get_data_path_by_offset(PartitionChunkOffset(offset as u32))?,
                    Some(v1_path_of(offset, path_len)),
                    "offset {offset}"
                );
            }
            Ok(())
        })
    }

    /// Four 80-byte paths. A 100-byte cap holds three hash rows, then one.
    /// Two paths are 160 bytes, so each range-copy write is one path.
    #[test]
    fn v1_range_copy_splits_when_the_next_path_passes_the_byte_cap() -> eyre::Result<()> {
        const N: u64 = 4;
        let run = run_v1_path_cap("convert-v1-range-split", N, 80, 100)?;
        assert_eq!(run.stats.intervals, 1, "{:?}", run.stats);
        assert_eq!(run.stats.data_paths, N, "{:?}", run.stats);
        assert_eq!(run.stats.skipped_frontier, 0, "{:?}", run.stats);
        assert_eq!(
            run.reads, 2,
            "first pass reads two paths; the rescan reads the hash page once"
        );
        assert_eq!(run.batches, vec![80, 80, 80, 80], "{:?}", run.batches);
        assert_v1_paths_stored(&run.rocks, N, 80)?;
        Ok(())
    }

    /// Seven 30-byte paths. Six hash rows fit in 200 bytes (180 path bytes)
    /// and stay one write. The seventh path is its own write.
    #[test]
    fn v1_range_copy_keeps_a_hash_batch_that_fits() -> eyre::Result<()> {
        const N: u64 = 7;
        let run = run_v1_path_cap("convert-v1-range-fit", N, 30, 200)?;
        assert_eq!(run.stats.data_paths, N, "{:?}", run.stats);
        assert_eq!(
            run.reads, 7,
            "first pass only; the rescan reads each hash page once"
        );
        assert_eq!(run.batches, vec![180, 30], "{:?}", run.batches);
        assert_v1_paths_stored(&run.rocks, N, 30)?;
        Ok(())
    }

    /// A path longer than the cap is still written. The next path is a new write.
    #[test]
    fn v1_range_copy_writes_a_path_larger_than_the_cap() -> eyre::Result<()> {
        const N: u64 = 2;
        let run = run_v1_path_cap("convert-v1-range-oversize", N, 120, 100)?;
        assert_eq!(run.stats.data_paths, N, "{:?}", run.stats);
        assert_eq!(
            run.reads, 2,
            "first pass only; the rescan reads the hash page once"
        );
        assert_eq!(run.batches, vec![120, 120], "{:?}", run.batches);
        assert_v1_paths_stored(&run.rocks, N, 120)?;
        Ok(())
    }

    /// Four 80-byte paths and a 100-byte cap. The hash rows are two pages.
    /// The first pass point-reads two paths. The rescan is one view per page.
    #[test]
    fn v1_range_copy_counts_one_view_per_hash_page() -> eyre::Result<()> {
        const N: u64 = 4;
        let run = run_v1_path_cap("convert-v1-range-page-views", N, 80, 100)?;
        assert_eq!(run.stats.data_paths, N, "{:?}", run.stats);
        assert_eq!(run.reads, 2, "first pass only");
        assert_eq!(
            run.copy_views, 2,
            "one view per hash page, not one per path; point reads {}",
            run.reads
        );
        assert!(
            run.copy_views < usize::try_from(N).unwrap_or(usize::MAX),
            "copy views {} counted a path, not a page",
            run.copy_views
        );
        assert_v1_paths_stored(&run.rocks, N, 80)?;
        Ok(())
    }

    /// The same run with one row per view. The copy counter must move with
    /// each view, and it must pass the first-pass point reads.
    #[test]
    fn v1_range_copy_counts_each_path_view_when_the_group_is_one() -> eyre::Result<()> {
        const N: u64 = 4;
        let run = run_v1_path_cap_grouped("convert-v1-range-path-views", N, 80, 100, Some(1))?;
        assert_eq!(run.stats.data_paths, N, "{:?}", run.stats);
        assert_eq!(run.reads, 2, "the group does not add point reads");
        assert!(
            run.copy_views > run.reads,
            "group 1 copy views {} are not above first-pass point reads {}",
            run.copy_views,
            run.reads
        );
        assert_eq!(run.copy_views, 4, "one view per path");
        assert_v1_paths_stored(&run.rocks, N, 80)?;
        Ok(())
    }

    /// Path hashes for a v2 batch are read in the same view as the data paths.
    #[test]
    fn verify_loads_path_hashes_in_the_data_path_view() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-verify-views")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(verify_views(), 1, "two proof rows share one source view");
        Ok(())
    }

    /// One v1 interval's data paths and offset metas are two views, not one per chunk.
    #[test]
    fn verify_v1_loads_each_path_batch_in_two_views() -> eyre::Result<()> {
        const N: u64 = 3;
        let root = temp("convert-verify-v1-views");
        let consensus = write_v1_one_tx_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(verify_views(), 2);
        Ok(())
    }

    /// A view per path row counts once per view.
    #[test]
    fn verify_v1_counts_each_path_batch_view() -> eyre::Result<()> {
        const N: u64 = 3;
        let root = temp("convert-verify-v1-views-per-row");
        let consensus = write_v1_one_tx_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let _group = set_v1_path_view_group(1);
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(verify_views(), 6, "{report}");
        Ok(())
    }

    /// Offsets whose hash row was already compared do not need another read.
    #[test]
    fn verify_v1_does_not_point_read_accepted_hash_rows() -> eyre::Result<()> {
        const N: u64 = 3;
        let root = temp("convert-verify-v1-point-reads");
        let consensus = write_v1_one_tx_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_v1_uncompared_hash_views();
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(
            v1_uncompared_hash_views(),
            0,
            "accepted hash rows are already compared"
        );
        assert_eq!(verify_views(), 2);
        Ok(())
    }

    /// Two checksum rows inside a v1 path hole are one hash-table read, not
    /// one read per offset. A verify before those rows exist does not read them.
    #[test]
    fn verify_v1_reads_uncompared_offsets_in_one_view() -> eyre::Result<()> {
        const N: u64 = 4;
        let holes = [1_u64, 2];
        let root = temp("convert-v1-uncompared-views");
        let consensus = write_v1_one_tx_with_holes(root.path(), N, &holes)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.data_paths, N - holes.len() as u64, "{stats:?}");

        reset_v1_uncompared_hash_views();
        reset_verify_views();
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!clean.has_mismatch(), "{clean}");
        assert_eq!(v1_uncompared_hash_views(), 0, "{clean}");
        assert_eq!(verify_views(), 2, "{clean}");

        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![
                    PackedOffsetRow {
                        offset: PartitionChunkOffset(holes[0] as u32),
                        crc: Some(0x11),
                        proof: None,
                    },
                    PackedOffsetRow {
                        offset: PartitionChunkOffset(holes[1] as u32),
                        crc: Some(0x22),
                        proof: None,
                    },
                ])?;
                Ok(())
            })?;
        }
        reset_v1_uncompared_hash_views();
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.mismatch_chunk_crc >= 2, "{report}");
        assert_eq!(
            v1_uncompared_hash_views(),
            1,
            "two uncompared offsets are one hash read, counter stayed {}",
            v1_uncompared_hash_views()
        );
        assert_eq!(
            verify_views(),
            3,
            "path compare is two views plus one hash batch"
        );
        Ok(())
    }

    /// A view per uncompared offset counts once per view.
    #[test]
    fn verify_v1_counts_each_uncompared_hash_view() -> eyre::Result<()> {
        const N: u64 = 4;
        let holes = [1_u64, 2];
        let root = temp("convert-v1-uncompared-per-key");
        let consensus = write_v1_one_tx_with_holes(root.path(), N, &holes)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![
                    PackedOffsetRow {
                        offset: PartitionChunkOffset(holes[0] as u32),
                        crc: Some(0x11),
                        proof: None,
                    },
                    PackedOffsetRow {
                        offset: PartitionChunkOffset(holes[1] as u32),
                        crc: Some(0x22),
                        proof: None,
                    },
                ])?;
                Ok(())
            })?;
        }
        let _group = set_v1_uncompared_hash_group(1);
        reset_v1_uncompared_hash_views();
        reset_verify_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.mismatch_chunk_crc >= 2, "{report}");
        assert_eq!(v1_uncompared_hash_views(), 2, "{report}");
        assert_eq!(verify_views(), 4, "{report}");
        Ok(())
    }

    /// Compared offsets merge only when they are adjacent. A hole stays out,
    /// and a dense run is one range.
    #[test]
    fn verify_v1_stores_accepted_offsets_as_merged_ranges() -> eyre::Result<()> {
        const N: u64 = 4;
        let root = temp("convert-verify-v1-accepted-ranges");
        let consensus = write_v1_one_tx_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_accepted_offset_ranges();
        reset_v1_uncompared_hash_views();
        let dense = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!dense.has_mismatch(), "{dense}");
        assert_eq!(v1_uncompared_hash_views(), 0);
        assert_eq!(
            accepted_offset_ranges(),
            1,
            "a dense run of {N} offsets is one range"
        );

        let hole_root = temp("convert-verify-v1-accepted-hole");
        let hole = 1_u64;
        let consensus = write_v1_one_tx_with_hole(hole_root.path(), N, hole)?;
        let mdbx = hole_root.path().join("db");
        let rocks = hole_root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_accepted_offset_ranges();
        reset_v1_uncompared_hash_views();
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!clean.has_mismatch(), "{clean}");
        assert_eq!(v1_uncompared_hash_views(), 0);
        assert_eq!(
            accepted_offset_ranges(),
            2,
            "the hole at {hole} splits the compared offsets"
        );

        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(hole as u32),
                    crc: Some(0x0102_0304),
                    proof: None,
                }])?;
                Ok(())
            })?;
        }
        reset_accepted_offset_ranges();
        let crc = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(crc.mismatch_chunk_crc >= 1, "{crc}");
        assert_eq!(accepted_offset_ranges(), 2, "{crc}");
        Ok(())
    }

    /// Placement-start checks share one source view per batch. A batch of one
    /// still reads the start and the previous offset together.
    #[test]
    fn verify_v1_checks_placement_starts_in_one_view() -> eyre::Result<()> {
        const N: u64 = 3;
        let root = temp("convert-verify-v1-placement-views");
        let consensus = write_v1_one_chunk_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        reset_v1_uncompared_hash_views();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(v1_uncompared_hash_views(), 0, "{report}");
        assert_eq!(
            v1_placement_views(),
            1,
            "three placement starts share one view"
        );

        let _batch = set_v1_placement_batch(1);
        reset_v1_uncompared_hash_views();
        let again = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!again.has_mismatch(), "{again}");
        assert_eq!(v1_uncompared_hash_views(), 0, "{again}");
        assert_eq!(
            v1_placement_views(),
            usize::try_from(N).unwrap_or(usize::MAX),
            "a batch of one is one view per placement"
        );
        Ok(())
    }

    /// Two placement starts in separate merge batches still share one tx-path cache.
    #[test]
    fn verify_keeps_one_tx_path_cache_across_placement_batches() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("convert-verify-placement-cache")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let _batch = set_merge_batch(1);
        reset_verify_placement_caches();
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(
            verify_placement_caches(),
            1,
            "placement batches must share one tx-path cache"
        );
        Ok(())
    }

    #[test]
    fn verify_v1_skip_set_covers_out_of_order_ranges() {
        let mut skips = super::FrontierSkips::default();
        skips.push(PartitionChunkOffset(10), PartitionChunkOffset(12));
        skips.push(PartitionChunkOffset(0), PartitionChunkOffset(8));
        skips.push(PartitionChunkOffset(4), PartitionChunkOffset(6));
        assert!(skips.covers(0));
        assert!(skips.covers(8));
        assert!(!skips.covers(9));
        assert!(skips.covers(11));
        assert!(!skips.covers(13));
        skips.push(PartitionChunkOffset(9), PartitionChunkOffset(9));
        assert!(skips.covers(9));
    }

    /// One-chunk v1 placements, one tx each, in a single block.
    fn write_v1_one_chunk_partition(root: &Path, n: u64) -> eyre::Result<PathBuf> {
        write_v1_one_chunk_partition_with(root, n, 4)
    }

    fn write_v1_one_chunk_partition_with(
        root: &Path,
        n: u64,
        data_len: usize,
    ) -> eyre::Result<PathBuf> {
        use crate::submodule::tables::v1_fixture::master;
        use crate::tx_path_derive::{TxPathBlockCache, derive_tx_path_at_offset_cached};
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let txs: Vec<_> = (0..n).map(|i| tx_at(i + 1, 32)).collect();
        let (consensus, opened, index) = open_v1_fixture(root, &txs, n)?;
        let mut cache = TxPathBlockCache::new();
        {
            let tx = index.tx_mut()?;
            for offset in 0..n {
                let derived = derive_tx_path_at_offset_cached(
                    &opened,
                    &mut cache,
                    DataLedger::Publish,
                    offset,
                    32,
                )?;
                let proof = derived.proof.proof;
                let path_hash = tx_path_hash(&proof);
                let mut data = vec![0xab; data_len.max(8)];
                data[..8].copy_from_slice(&offset.to_be_bytes());
                let data_hash = H256::from(hash_sha256(&data));
                let key = PartitionChunkOffset(offset as u32);
                tx.put::<master::ChunkPathHashesByOffset>(
                    key,
                    ChunkPathHashes {
                        data_path_hash: Some(data_hash),
                        tx_path_hash: Some(path_hash),
                    },
                )?;
                tx.put::<master::ChunkDataPathByPathHash>(data_hash, data)?;
                tx.put::<master::TxPathByTxPathHash>(path_hash, proof)?;
                tx.put::<master::DataRootInfosByDataRoot>(
                    derived.data_root,
                    DataRootInfos(vec![DataRootInfo {
                        start_offset: RelativeChunkOffset(
                            i32::try_from(offset).unwrap_or(i32::MAX),
                        ),
                        data_size: 32,
                    }]),
                )?;
            }
            tx.commit()?;
        }
        drop(index);
        drop(opened);
        Ok(consensus)
    }

    #[test]
    fn v1_one_chunk_placements_share_a_batch() -> eyre::Result<()> {
        let root = temp("convert-v1-batch");
        let consensus = write_v1_one_chunk_partition(root.path(), 3)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = 3;
        options.batch_rows = 8;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert!(!rocks.join("CONVERTING").exists());
        assert_eq!(stats.intervals, 3);
        assert_eq!(stats.data_paths, 3);
        assert_eq!(stats.skipped_frontier, 0);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        let opened = RocksSubmoduleStore::open(&rocks)?;
        opened.view(|tx| {
            let place = tx
                .placement_at(PartitionChunkOffset(2))?
                .expect("third placement");
            assert_eq!(place.tx_index, Some(2));
            assert_eq!(place.end, PartitionChunkOffset(2));
            assert!(
                tx.get_data_path_by_offset(PartitionChunkOffset(2))?
                    .is_some()
            );
            Ok(())
        })?;
        Ok(())
    }

    /// The sealed directory has no recycled WAL files.
    #[test]
    fn convert_large_paths_leave_no_wal() -> eyre::Result<()> {
        const N: u64 = 2_500;
        const DATA_LEN: usize = 32 * 1024;
        let root = temp("convert-wal");
        let consensus = write_v1_one_chunk_partition_with(root.path(), N, DATA_LEN)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.intervals, N, "{stats}");
        assert_eq!(stats.data_paths, N, "{stats}");
        let (dir_bytes, wal_bytes, wal_files) = wal_snapshot(&rocks);
        assert!(
            wal_bytes <= WAL_BYTE_BOUND,
            "dir_bytes={dir_bytes} wal_bytes={wal_bytes} wal_files={wal_files}"
        );
        let opened = RocksSubmoduleStore::open(&rocks)?;
        assert!(
            opened
                .view(|tx| tx.placement_at(PartitionChunkOffset((N - 1) as u32)))?
                .is_some()
        );
        drop(opened);
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
        Ok(())
    }

    #[test]
    fn in_progress_v1_migration_is_refused() -> eyre::Result<()> {
        use crate::metadata::MetadataKey;
        use crate::submodule::tables::v1_fixture::master;
        use reth_db::Database as _;
        use reth_db::transaction::{DbTx as _, DbTxMut as _};

        let root = temp("convert-migrate-blob");
        let consensus = root.path().join("consensus");
        write_chain(&consensus)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let db = open_or_create_db(&mdbx, master::MasterV1Tables::ALL, testing_args()?)?;
        {
            let tx = db.tx_mut()?;
            let blob = format!("{}copy 0 0 0 0 0 0\n", super::migrate::PROGRESS_PREFIX);
            tx.put::<master::Metadata>(MetadataKey::DBSchemaVersion, blob.into_bytes())?;
            tx.commit()?;
        }
        drop(db);
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?).unwrap_err();
        assert!(format!("{err:#}").contains("in-progress"), "{err:#}");
        assert!(!rocks.exists());
        Ok(())
    }

    #[test]
    fn convert_refuses_when_an_opener_holds_the_lock() -> eyre::Result<()> {
        if let Ok(kind) = std::env::var("IRYS_CONVERT_LOCK_CHILD") {
            return hold_index_lock(&kind);
        }
        let Populated {
            root,
            consensus,
            mdbx,
            rocks,
            options,
        } = populated("convert-lock")?;

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
        assert_rocks(&RocksSubmoduleStore::open(&rocks)?, &consensus)?;
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

    /// On-disk families a RocksDB v1 index stores. `default` is listed too.
    const INDEX_FAMILIES: [&str; 6] = [
        "ChunkMetaByOffset",
        "DataRootPlacements",
        "PendingBodyMigrations",
        "PlacementIntervals",
        "Metadata",
        "ChunkOffsetsByContentHash",
    ];

    /// Metadata key this build does not define. A stored row is still compared.
    const EXTRA_METADATA_KEY: [u8; 4] = (6_u32).to_le_bytes();

    fn family_mismatches(report: &super::EngineEquivalenceReport, name: &str) -> u64 {
        report
            .family_mismatches
            .iter()
            .find(|row| row.family == name)
            .unwrap_or_else(|| panic!("no count for {name}: {report}"))
            .mismatches
    }

    fn seed_rocks_families(dir: &Path) -> eyre::Result<()> {
        fs::create_dir_all(dir)?;
        {
            let store = RocksSubmoduleStore::open(dir)?;
            let path_hash = H256::repeat_byte(4);
            let data_root = H256::repeat_byte(5);
            store.update(|tx| {
                tx.add_tx_leaf_binding(
                    path_hash,
                    &TxLeafBinding {
                        data_root,
                        prefix_hash: H256::repeat_byte(1),
                    },
                )?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(3),
                    Some(path_hash),
                )?;
                tx.add_data_root_info(
                    data_root,
                    &DataRootInfo {
                        start_offset: RelativeChunkOffset(1),
                        data_size: 64,
                    },
                )?;
                tx.add_pending_body_migration(
                    PartitionChunkOffset::from(0),
                    &PendingBodyMigration {
                        data_root,
                        data_size: 64,
                        start_offset: RelativeChunkOffset(1),
                        block_height: 1,
                        attempts: 0,
                    },
                )?;
                tx.add_full_data_path(PartitionChunkOffset::from(0), indexed_data_leaf(1))?;
                Ok(())
            })?;
            store.bind_packing_identity(
                Some(H256::repeat_byte(7)),
                &crate::submodule::packing::PackingRecord {
                    packing_address: [9_u8; 20],
                    iterations: 10,
                    chunk_size: 262_144,
                    chain_id: 1270,
                },
            )?;
        }
        put_metadata_bytes(dir, &EXTRA_METADATA_KEY, &[0_u8; 24])?;
        Ok(())
    }

    fn copy_tree(src: &Path, dst: &Path) -> eyre::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let to = dst.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &to)?;
            } else {
                fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    fn refresh_from(src: &Path, dst: &Path) -> eyre::Result<()> {
        if dst.exists() {
            fs::remove_dir_all(dst)?;
        }
        copy_tree(src, dst)
    }

    fn open_rocks_rw(dir: &Path) -> eyre::Result<rocksdb::DB> {
        let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), dir)?;
        let mut db_opts = rocksdb::Options::default();
        db_opts.create_if_missing(false);
        db_opts.create_missing_column_families(false);
        let families = names
            .into_iter()
            .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, rocksdb::Options::default()));
        rocksdb::DB::open_cf_descriptors(&db_opts, dir, families)
            .wrap_err("open rocksdb to change one byte")
    }

    fn put_metadata_bytes(dir: &Path, key: &[u8], value: &[u8]) -> eyre::Result<()> {
        let db = open_rocks_rw(dir)?;
        let handle = db
            .cf_handle("Metadata")
            .ok_or_else(|| eyre::eyre!("Metadata family missing"))?;
        db.put_cf(&handle, key, value)
            .wrap_err("write metadata bytes")?;
        Ok(())
    }

    /// Change one stored byte in `family`. `key` selects that row. Otherwise the first row.
    fn flip_one_stored_byte(dir: &Path, family: &str, key: Option<&[u8]>) -> eyre::Result<()> {
        let db = open_rocks_rw(dir)?;
        let handle = db
            .cf_handle(family)
            .ok_or_else(|| eyre::eyre!("missing column family {family}"))?;
        let mut opts = rocksdb::ReadOptions::default();
        opts.set_total_order_seek(true);
        let mut iter = db.raw_iterator_cf_opt(&handle, opts);
        match key {
            Some(key) => iter.seek(key),
            None => iter.seek_to_first(),
        }
        let found = iter.key().map(<[u8]>::to_vec);
        let Some(found) = found else {
            eyre::bail!("column family {family} has no row to change");
        };
        if let Some(key) = key
            && found.as_slice() != key
        {
            eyre::bail!("column family {family} has no row for the requested key");
        }
        let mut value = iter.value().unwrap_or(&[]).to_vec();
        iter.status().wrap_err("read column family")?;
        drop(iter);
        if value.is_empty() {
            value.push(1);
        } else {
            value[0] ^= 0x01;
        }
        db.put_cf(&handle, found, value)
            .wrap_err("write the changed row")?;
        Ok(())
    }

    #[test]
    fn verify_rocks_counts_one_changed_byte() -> eyre::Result<()> {
        use crate::metadata::MetadataKey;
        use reth_db::table::Encode as _;

        let root = temp("verify-rocks-families");
        let left = root.path().join("left");
        let right = root.path().join("right");
        seed_rocks_families(&left)?;
        refresh_from(&left, &right)?;
        let equal = verify_rocks_records(&left, &right)?;
        assert!(!equal.has_mismatch(), "{equal}");
        for name in INDEX_FAMILIES {
            assert_eq!(family_mismatches(&equal, name), 0, "{equal}");
        }
        assert_eq!(family_mismatches(&equal, "default"), 0, "{equal}");

        let assert_one = |family: &str, report: &super::EngineEquivalenceReport| {
            assert_eq!(family_mismatches(report, family), 1, "{family}: {report}");
            assert!(report.has_mismatch(), "{family}: {report}");
            for other in INDEX_FAMILIES {
                if other != family {
                    assert_eq!(
                        family_mismatches(report, other),
                        0,
                        "{family} also changed {other}: {report}"
                    );
                }
            }
        };
        for family in INDEX_FAMILIES {
            refresh_from(&left, &right)?;
            flip_one_stored_byte(&right, family, None)?;
            assert_one(family, &verify_rocks_records(&left, &right)?);
        }
        let keys = [
            MetadataKey::DBSchemaVersion.encode(),
            MetadataKey::PartitionHash.encode(),
            MetadataKey::PackingRecord.encode(),
            EXTRA_METADATA_KEY,
        ];
        for key in keys {
            refresh_from(&left, &right)?;
            flip_one_stored_byte(&right, "Metadata", Some(&key))?;
            assert_one("Metadata", &verify_rocks_records(&left, &right)?);
        }
        Ok(())
    }

    #[test]
    fn verify_flags_an_invented_checksum() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("verify-invented-crc")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!clean.has_mismatch(), "{clean}");
        assert_eq!(clean.mismatch_chunk_crc, 0, "{clean}");
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            let proof = opened
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset(0)))?
                .expect("source proof");
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(0),
                    crc: Some(0x0102_0304),
                    proof: Some(proof),
                }])?;
                Ok(())
            })?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.mismatch_chunk_crc >= 1, "{report}");
        assert_eq!(report.mismatch_data_path, 0, "{report}");
        assert!(report.intervals_checked >= 1, "{report}");
        assert!(report.has_mismatch(), "{report}");
        Ok(())
    }

    /// After the node writes checksums on RocksDB, a rocks-to-mdbx copy has
    /// none. Forward verify flags that; rollback verify reports it as dropped.
    #[test]
    fn rollback_verify_accepts_a_copy_that_dropped_node_checksums() -> eyre::Result<()> {
        let Populated {
            root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("verify-rollback-crc")?;
        let back = root.path().join("back");
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            let proof = opened
                .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset(0)))?
                .expect("source proof");
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(0),
                    crc: Some(0x0102_0304),
                    proof: Some(proof),
                }])?;
                Ok(())
            })?;
        }
        convert_rocks_to_mdbx(&rocks, &back, &options)?;
        let forward = verify_mdbx_and_rocks(&back, &rocks, &options)?;
        assert!(forward.has_mismatch(), "{forward}");
        let mut rollback = options;
        rollback.rollback = true;
        let report = verify_mdbx_and_rocks(&back, &rocks, &rollback)?;
        assert!(!report.has_mismatch(), "{report}");
        assert!(report.dropped_checksums >= 1, "{report}");
        assert_eq!(report.mismatch_data_path, 0, "{report}");

        // A dropped checksum must not hide another family.
        {
            let copy = MdbxSubmoduleStore::open(&back, testing_args()?)?;
            copy.update(|tx| tx.add_full_data_path(PartitionChunkOffset(0), b"changed".to_vec()))?;
        }
        let changed = verify_mdbx_and_rocks(&back, &rocks, &rollback)?;
        assert!(changed.has_mismatch(), "{changed}");
        assert!(changed.mismatch_data_path >= 1, "{changed}");
        assert!(changed.dropped_checksums >= 1, "{changed}");
        Ok(())
    }

    /// A placement that covers a v1 offset with no data path is not equality.
    /// A later checksum or proof at that hole is a mismatch. The same verify
    /// is clean before the node writes the row. Audit still allows a
    /// checksum inside a placement, so the mismatch has to come from verify.
    #[test]
    fn verify_v1_flags_a_row_inside_a_path_hole() -> eyre::Result<()> {
        const N: u64 = 3;
        const HOLE: u32 = 1;
        let root = temp("convert-v1-path-hole");
        let consensus = write_v1_one_tx_with_hole(root.path(), N, u64::from(HOLE))?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.data_paths, N - 1, "{stats}");
        reset_v1_uncompared_hash_views();
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!clean.has_mismatch(), "{clean}");
        assert_eq!(v1_uncompared_hash_views(), 0, "{clean}");
        assert_eq!(clean.index_audit_findings, 0, "{clean}");

        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(HOLE),
                    crc: Some(0x0102_0304),
                    proof: None,
                }])?;
                Ok(())
            })?;
        }
        let crc = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(
            crc.has_mismatch(),
            "checksum-only row inside a path hole must fail verify: {crc}"
        );
        assert!(crc.mismatch_chunk_crc >= 1, "{crc}");
        assert_eq!(crc.index_audit_findings, 0, "{crc}");

        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(HOLE),
                    crc: None,
                    proof: Some(b"hole-proof".to_vec()),
                }])?;
                Ok(())
            })?;
        }
        let proof = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(
            proof.has_mismatch(),
            "proof inside a path hole must fail verify: {proof}"
        );
        assert!(proof.mismatch_data_path >= 1, "{proof}");
        assert_eq!(proof.mismatch_chunk_crc, 0, "{proof}");
        assert_eq!(proof.index_audit_findings, 0, "{proof}");
        Ok(())
    }

    #[test]
    fn verify_flags_a_checksum_only_row_the_source_lacks() -> eyre::Result<()> {
        let Populated {
            root: _root,
            consensus: _,
            mdbx,
            rocks,
            options,
        } = populated("verify-crc-only")?;
        convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        {
            let opened = RocksSubmoduleStore::open(&rocks)?;
            opened.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset(50),
                    crc: Some(7),
                    proof: None,
                }])?;
                Ok(())
            })?;
        }
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(report.mismatch_chunk_crc >= 1, "{report}");
        assert!(report.intervals_checked >= 1, "{report}");
        assert!(report.offsets_checked >= 1, "{report}");
        assert!(report.has_mismatch(), "{report}");
        Ok(())
    }

    #[test]
    fn verify_rocks_records_counts_one_offset_crc_difference() -> eyre::Result<()> {
        fn write_crc(dir: &Path, crc: u32) -> eyre::Result<()> {
            let store = RocksSubmoduleStore::open(dir)?;
            store.update(|tx| {
                tx.write_packed_offset_rows(vec![PackedOffsetRow {
                    offset: PartitionChunkOffset::from(1),
                    crc: Some(crc),
                    proof: None,
                }])?;
                Ok(())
            })?;
            drop(store);
            Ok(())
        }

        let left = temp("v32_crc_left");
        let right = temp("v32_crc_right");
        let same = temp("v32_crc_same");
        write_crc(left.path(), 0x1111_1111)?;
        write_crc(right.path(), 0x2222_2222)?;
        write_crc(same.path(), 0x1111_1111)?;
        let differ = verify_rocks_records(left.path(), right.path())?;
        assert!(differ.offsets_checked > 0, "{differ}");
        assert!(differ.mismatch_chunk_crc > 0, "{differ}");
        let equal = verify_rocks_records(left.path(), same.path())?;
        assert_eq!(equal.mismatch_chunk_crc, 0, "{equal}");
        Ok(())
    }

    fn park_until_killed() {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(120) {
            thread::sleep(Duration::from_millis(100));
        }
    }
}
