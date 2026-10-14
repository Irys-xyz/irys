//! Offline copy between submodule indexes.
//!
//! `mdbx-to-rocks` writes a RocksDB v1 index.
//! `rocks-to-mdbx` rebuilds an MDBX schema v2 index from RocksDB v1. Each
//! stored tx path must equal the path derived from the consensus database.
//! A mismatch aborts and names the partition offset.
//!
//! The node must be stopped. This tool takes the engine lock (MDBX exclusive
//! open, RocksDB `LOCK`) and refuses the run when that lock is already held.
//! It does not require the directory name to contain `copy`.
//!
//! The source is never modified. A schema v1 MDBX directory is read as v1
//! and converted directly. The completion marker is written last: the
//! RocksDB `SCHEMA` file, or the MDBX schema row [`SUBMODULE_SCHEMA`].
//! A crash before that marker leaves a `CONVERTING` file and no marker.
//! A finished index removes `CONVERTING` after the marker is durable.
//! A crash between the two leaves both files. The marker still protects
//! the directory. `--replace-incomplete` deletes only a directory that
//! contains `CONVERTING` and none of the protected names (a storage module,
//! a consensus database, or a finished index).
//!
//! Rows are written in bounded batches. The RocksDB target disables the WAL.
//! The seal flush, then the SCHEMA file, makes that copy durable. An MDBX
//! target commits each batch in durable sync mode. Its schema row stays
//! incomplete until the last commit. Verification compares stored bytes
//! with a fresh tx-path derive.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eyre::WrapErr as _;
use irys_types::{
    ChunkDataPath, DataLedger, DataTransactionHeader, H256, PartitionChunkOffset,
    RelativeChunkOffset, TxPath, TxPathHash, hash_sha256,
};
use reth_db::cursor::DbCursorRO as _;
use reth_db::mdbx::DatabaseArguments;
use reth_db::mdbx::Error as MdbxError;
use reth_db::table::Table as _;
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
    /// Source rows outside every interval. The node drops these at open.
    pub skipped_orphans: u64,
    /// Placements past the consensus frontier. The node has not indexed them yet.
    /// A non-zero count is a mismatch: verify must not accept a partial index.
    pub skipped_frontier: u64,
    /// RocksDB v1 rows `audit_index` rejected. Zero on MDBX and schema v2.
    pub index_audit_findings: u64,
    /// RocksDB v1 checksum that is not the value verify expected.
    pub mismatch_chunk_crc: u64,
    /// Byte mismatches per column family. Every on-disk family is listed, including a zero count.
    pub family_mismatches: Vec<FamilyMismatch>,
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
            || self.content_offset_mismatches > 0
            || self.mismatch_data_path_hash > 0
            || self.mismatch_tx_path_hash > 0
            || self.skipped_frontier > 0
            || self.index_audit_findings > 0
            || self.mismatch_chunk_crc > 0
            || self.family_mismatches.iter().any(|row| row.mismatches > 0)
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
        writeln!(
            f,
            "content_offsets_checked: {}",
            self.content_offsets_checked
        )?;
        writeln!(
            f,
            "content_offset_mismatches: {}",
            self.content_offset_mismatches
        )?;
        writeln!(f, "skipped_orphans: {}", self.skipped_orphans)?;
        writeln!(f, "skipped_frontier: {}", self.skipped_frontier)?;
        writeln!(f, "index_audit_findings: {}", self.index_audit_findings)?;
        writeln!(f, "mismatch_chunk_crc: {}", self.mismatch_chunk_crc)?;
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
        writeln!(f, "migrated_v1: {}", self.migrated_v1)?;
        writeln!(f, "source_v1: {}", self.source_v1)?;
        writeln!(f, "tx_paths: {}", self.tx_paths)?;
        writeln!(f, "tx_leaf_bindings: {}", self.tx_leaf_bindings)?;
        writeln!(f, "intervals: {}", self.intervals)?;
        writeln!(f, "path_hashes: {}", self.path_hashes)?;
        writeln!(f, "data_paths: {}", self.data_paths)?;
        writeln!(f, "data_roots: {}", self.data_roots)?;
        writeln!(f, "pending: {}", self.pending)?;
        writeln!(f, "reset_marker: {}", self.reset_marker)?;
        writeln!(f, "skipped_orphans: {}", self.skipped_orphans)?;
        writeln!(f, "skipped_frontier: {}", self.skipped_frontier)?;
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
    /// Validate, scan, and report. Do not create or write the target.
    pub dry_run: bool,
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
        // MDBX stores the raw proof under this name. RocksDB v1 reads
        // `ChunkMetaByOffset` through `read_offset_row_keys`.
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
            // MDBX. RocksDB v1 offset keys use `read_offset_row_keys`.
            OffsetTable::DataPaths => ChunkDataPathByOffset::NAME,
            OffsetTable::Pending => PendingBodyMigrationsByOffset::NAME,
        };
        self.read_cf_keys(name, after, limit)
    }

    fn highest_placement_end(&self) -> eyre::Result<Option<PartitionChunkOffset>> {
        Self::highest_placement_end(self)
    }
}

/// Copies a v1 or v2 MDBX submodule index into a new RocksDB v1 directory.
///
/// The source is opened read-only. A v1 index is converted from its own rows.
/// It is not migrated. Each stored tx path must match the path derived from
/// the consensus database. A mismatch aborts and names the partition offset.
/// The RocksDB `SCHEMA` file is written only after the rows are durable.
pub fn convert_mdbx_to_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
    let started = Instant::now();
    guard_paths(mdbx_dir, rocks_dir, options)?;
    let map = require_map(options)?;
    let db = open_consensus(options)?;
    if !mdbx_dir.join("mdbx.dat").is_file() {
        eyre::bail!(
            "source {} has no mdbx.dat. Pass the submodule db/ directory.",
            mdbx_dir.display()
        );
    }
    let opened = map_open(open_mdbx_source(mdbx_dir, exclusive_args(options)))?;
    let source_v1 = matches!(opened, OpenedMdbx::V1(_));
    if source_reset(&opened)? {
        return Ok(finish_stats(
            write_reset_rocks(rocks_dir, options, source_v1)?,
            started,
            source_v1,
        ));
    }
    let stats = match &opened {
        OpenedMdbx::V2(src) => copy_v2_rocks(src, rocks_dir, options, &db, &map)?,
        OpenedMdbx::V1(env) => copy_v1_rocks(env, rocks_dir, options, &db, &map)?,
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
pub fn convert_rocks_to_mdbx(
    rocks_dir: &Path,
    mdbx_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<ConvertStats> {
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
        prepare_mdbx_target(mdbx_dir, args.clone(), options.replace_incomplete)?;
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
pub fn verify_mdbx_and_rocks(
    mdbx_dir: &Path,
    rocks_dir: &Path,
    options: &ConvertOptions,
) -> eyre::Result<EngineEquivalenceReport> {
    if options.dry_run {
        eyre::bail!("--dry-run is not valid for verify");
    }
    guard_paths(mdbx_dir, rocks_dir, options)?;
    let map = require_map(options)?;
    let db = open_consensus(options)?;
    require_rocks_v1(rocks_dir)?;
    let opened = map_open(open_mdbx_source(mdbx_dir, exclusive_args(options)))?;
    let rocks = map_open(RocksSubmoduleStore::open_read_only(rocks_dir))?;
    match opened {
        OpenedMdbx::V2(mdbx) => {
            if mdbx.reset_in_progress()? {
                verify_reset_target(true, &rocks)
            } else {
                let schema = mdbx.schema_bytes()?;
                verify_mdbx_rows_against_rocks(&mdbx, &rocks, schema, &db, &map)
            }
        }
        OpenedMdbx::V1(env) => {
            if reset_marker_on(&env)? {
                verify_reset_target(true, &rocks)
            } else {
                verify_mdbx_v1_against_rocks(&env, &rocks, &db, &map)
            }
        }
        OpenedMdbx::Empty(env) => verify_empty_and_rocks(&env, &rocks),
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
    let data_paths = copy_rows(
        |after| src.read_data_paths(after, limits),
        |batch| {
            let mut kept = Vec::new();
            for (offset, path) in batch {
                if src.view(|tx| tx.tx_interval_at(offset))?.is_none() {
                    skipped_orphans = skipped_orphans.saturating_add(1);
                    continue;
                }
                if skips.covers(offset.0) {
                    skipped_frontier = skipped_frontier.saturating_add(1);
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
                        tx.add_full_data_path(offset, path)?;
                    }
                    Ok(())
                })?;
            }
            Ok(copied)
        },
        &mut progress,
    )?;
    let pending = copy_rows(
        |after| src.read_pending(after, limits),
        |batch| {
            let mut kept = Vec::new();
            for (offset, job) in batch {
                if src.view(|tx| tx.tx_interval_at(offset))?.is_none() {
                    skipped_orphans = skipped_orphans.saturating_add(1);
                    continue;
                }
                if skips.covers(offset.0) {
                    skipped_frontier = skipped_frontier.saturating_add(1);
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
                if !covered {
                    skipped_orphans = skipped_orphans.saturating_add(1);
                    continue;
                }
                if skips.covers(offset.0) {
                    skipped_frontier = skipped_frontier.saturating_add(1);
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
                if !covered {
                    skipped_orphans = skipped_orphans.saturating_add(1);
                    continue;
                }
                if skips.covers(offset.0) {
                    skipped_frontier = skipped_frontier.saturating_add(1);
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
    let stored = src
        .view(|tx| tx.get_full_tx_path(interval.tx_path_hash))?
        .ok_or_else(|| eyre::eyre!("partition offset {} has no stored tx path", start.0))?;
    let binding = src.view(|tx| tx.get_tx_leaf_binding(interval.tx_path_hash))?;
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
) -> eyre::Result<EngineEquivalenceReport>
where
    S: RowSource + SubmoduleStore,
{
    let mut report = EngineEquivalenceReport::default();
    verify_data_paths(v2, v3, &mut report)?;
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
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()>
where
    S: RowSource + SubmoduleStore,
{
    merge_keys(
        &mut |after| v2.read_offset_keys(OffsetTable::DataPaths, after, VERIFY_KEYS),
        &mut |after| v3.read_offset_row_keys(after, VERIFY_KEYS),
        |keys| {
            let left = v2.view(|tx| read_data_paths_at(tx, keys))?;
            let right = v3.view(|tx| read_data_paths_at(tx, keys))?;
            let right_meta = v3.view(|tx| {
                keys.iter()
                    .map(|key| tx.get_offset_meta(*key))
                    .collect::<eyre::Result<Vec<_>>>()
            })?;
            for (index, key) in keys.iter().enumerate() {
                if left[index].is_some()
                    && right[index].is_none()
                    && v2.view(|tx| tx.tx_interval_at(*key))?.is_none()
                {
                    report.skipped_orphans = report.skipped_orphans.saturating_add(1);
                    continue;
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
                    report,
                );
                if let Some(path) = &left[index] {
                    let digest = H256::from(hash_sha256(path));
                    let stored = v2
                        .view(|tx| tx.get_path_hashes_by_offset(*key))?
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
    report: &mut EngineEquivalenceReport,
) {
    let Some(target) = target else {
        return;
    };
    if target.crc.is_none() {
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
            let mut cache = TxPathBlockCache::new();
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
    let filled = place.tx_start_offset.0 != 0 || place.data_size != 0;
    if filled {
        let base = ledger_base(map.slot_index, map.chunks_per_partition);
        let expected = i64::try_from(derived.tx_ledger_start)
            .ok()
            .zip(base)
            .and_then(|(start, base)| start.checked_sub(base));
        let start_ok =
            expected.and_then(|value| i32::try_from(value).ok()) == Some(place.tx_start_offset.0);
        if !start_ok || place.data_size != header.data_size {
            report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
            note(report, "data_root", format!("offset {} chain", key.0));
        }
    }
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
    let v3_ok = v3_schema.as_deref() == Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes());
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
    let Some(left) = canonical_existing(left) else {
        return false;
    };
    let Some(right) = canonical_existing(right) else {
        return false;
    };
    if left == right {
        return false;
    }
    left.starts_with(&right) || right.starts_with(&left)
}

/// First ledger offset of the submodule's slot, `slot * chunks_per_partition`.
fn ledger_base(slot_index: u64, chunks_per_partition: u64) -> Option<i64> {
    let slot = i64::try_from(slot_index).ok()?;
    let chunks = i64::try_from(chunks_per_partition).ok()?;
    slot.checked_mul(chunks)
}

/// Canonical path, including components that do not exist yet.
fn canonical_existing(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
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

fn finish_stats(mut stats: ConvertStats, started: Instant, source_v1: bool) -> ConvertStats {
    stats.source_v1 = source_v1;
    stats.migrated_v1 = false;
    stats.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    stats
}

#[derive(Default)]
struct FrontierSkips {
    ranges: Vec<(u32, u32)>,
}

impl FrontierSkips {
    fn covers(&self, offset: u32) -> bool {
        self.ranges
            .iter()
            .any(|&(start, end)| start <= offset && offset <= end)
    }

    fn push(&mut self, start: PartitionChunkOffset, end: PartitionChunkOffset) {
        self.ranges.push((start.0, end.0));
    }
}

enum OpenedMdbx {
    V2(MdbxSubmoduleStore),
    V1(DatabaseEnv),
    Empty(DatabaseEnv),
}

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
    let DatabaseError::Open(info) = err else {
        return false;
    };
    *info == MdbxError::NotFound.into()
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
    ensure_consensus_covers(
        db,
        map,
        src.highest_placement_end()?,
        options.allow_unresolved,
    )?;
    if options.dry_run {
        let stats = copy_mdbx_into_rocks(src, None, options, db, map)?;
        refuse_unresolved(&stats, options)?;
        return Ok(stats);
    }
    prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting(dir))?;
    let stats = copy_mdbx_into_rocks(src, Some(&dst), options, db, map)?;
    refuse_unresolved(&stats, options)?;
    seal_rocks_target(dst, options)?;
    clear_converting_sentinel(dir)?;
    Ok(stats)
}

fn write_empty_rocks(dir: &Path, options: &ConvertOptions) -> eyre::Result<ConvertStats> {
    if options.dry_run {
        return Ok(ConvertStats::default());
    }
    prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting(dir))?;
    if options.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
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
    prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting(dir))?;
    if options.fail_after_batches == Some(0) {
        eyre::bail!("injected conversion interrupt after 0 batches");
    }
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
    prepare_mdbx_target(dir, args.clone(), options.replace_incomplete)?;
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
    ensure_consensus_covers(
        db,
        map,
        highest_v1_placement(env)?,
        options.allow_unresolved,
    )?;
    if options.dry_run {
        let stats = copy_mdbx_v1_into_rocks(env, None, options, db, map)?;
        refuse_unresolved(&stats, options)?;
        return Ok(stats);
    }
    prepare_rocks_target(dir, options.replace_incomplete)?;
    let dst = map_open(RocksSubmoduleStore::open_converting(dir))?;
    let stats = copy_mdbx_v1_into_rocks(env, Some(&dst), options, db, map)?;
    refuse_unresolved(&stats, options)?;
    seal_rocks_target(dst, options)?;
    clear_converting_sentinel(dir)?;
    Ok(stats)
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
    /// The path bytes were larger than one batch. Write the placement, then
    /// scan this range once. Short runs keep `paths` and do not scan again.
    rescan: bool,
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
                    close_v1_run(
                        &mut src,
                        &mut sink,
                        run,
                        std::mem::take(&mut paths),
                        path_bytes,
                        rescan,
                    )?;
                }
                break;
            }
            for (offset, hashes) in &batch {
                let mut closed = Vec::new();
                migrate::absorb(&mut open, offset.0, hashes.tx_path_hash, &mut closed);
                if let Some(run) = closed.pop() {
                    let taken_bytes = path_bytes;
                    let taken_rescan = rescan;
                    path_bytes = 0;
                    rescan = false;
                    close_v1_run(
                        &mut src,
                        &mut sink,
                        run,
                        std::mem::take(&mut paths),
                        taken_bytes,
                        taken_rescan,
                    )?;
                }
                if hashes.tx_path_hash.is_none() {
                    if let Some(hash) = hashes.data_path_hash {
                        let _path = read_v1_data_path(env, *offset, hash)?;
                        orphans = orphans.saturating_add(1);
                    }
                } else if let Some(hash) = hashes.data_path_hash {
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
    _path_bytes: usize,
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
                    rescan: false,
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
                rescan: false,
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
    if !ready.rescan {
        sink.stats.data_paths = sink
            .stats
            .data_paths
            .saturating_add(u64::try_from(ready.paths.len()).unwrap_or(u64::MAX));
        sink.stats.path_hashes = sink.stats.data_paths;
    }
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

fn read_v1_data_path(
    env: &DatabaseEnv,
    offset: PartitionChunkOffset,
    hash: H256,
) -> eyre::Result<ChunkDataPath> {
    env.view_eyre(|tx| match tx.get::<ChunkDataPathByPathHash>(hash) {
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
    })
}

fn read_v1_tx_path(env: &DatabaseEnv, hash: H256) -> eyre::Result<Option<TxPath>> {
    env.view_eyre(|tx| match tx.get::<TxPathByTxPathHash>(hash) {
        Ok(path) => Ok(path),
        Err(err) if table_missing(&err) => Ok(None),
        Err(err) => Err(err.into()),
    })
}

fn read_v1_binding(env: &DatabaseEnv, hash: H256) -> eyre::Result<Option<TxLeafBinding>> {
    env.view_eyre(|tx| match tx.get::<TxLeafBindingByTxPathHash>(hash) {
        Ok(binding) => Ok(binding),
        Err(err) if table_missing(&err) => Ok(None),
        Err(err) => Err(err.into()),
    })
}

fn v1_infos(env: &DatabaseEnv, data_root: H256) -> eyre::Result<Vec<DataRootInfo>> {
    env.view_eyre(|tx| match tx.get::<DataRootInfosByDataRoot>(data_root) {
        Ok(Some(DataRootInfos(infos))) => Ok(infos),
        Ok(None) => Ok(Vec::new()),
        Err(err) if table_missing(&err) => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    })
}

fn copy_v1_range_paths(
    env: &DatabaseEnv,
    dst: Option<&RocksSubmoduleStore>,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    limits: Limits,
    progress: &mut CopyProgress,
) -> eyre::Result<u64> {
    let mut after = start.0.checked_sub(1).map(PartitionChunkOffset);
    let mut copied = 0_u64;
    loop {
        let batch = read_v1_hash_batch(env, after, limits)?;
        if batch.is_empty() {
            break;
        }
        let mut kept = Vec::new();
        let mut passed = false;
        for (offset, hashes) in &batch {
            if *offset > end {
                passed = true;
                break;
            }
            if let Some(hash) = hashes.data_path_hash {
                kept.push((*offset, read_v1_data_path(env, *offset, hash)?));
            }
        }
        let last = batch.last().map(|(key, _)| *key);
        if !kept.is_empty() {
            let n = u64::try_from(kept.len()).unwrap_or(u64::MAX);
            if let Some(dst) = dst {
                dst.update(|tx| {
                    for (offset, path) in kept {
                        require_interval(tx, offset, "data path")?;
                        tx.add_full_data_path(offset, path)?;
                    }
                    Ok(())
                })?;
                progress.note()?;
            }
            copied = copied.saturating_add(n);
        }
        if passed || last.is_some_and(|key| key >= end) {
            break;
        }
        after = last;
    }
    Ok(copied)
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

fn v1_hashes_at(
    env: &DatabaseEnv,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<ChunkPathHashes>> {
    env.view_eyre(|tx| match tx.get::<ChunkPathHashesByOffset>(offset) {
        Ok(row) => Ok(row),
        Err(err) if table_missing(&err) => Ok(None),
        Err(err) => Err(err.into()),
    })
}

fn v1_interval_starts_at(env: &DatabaseEnv, offset: PartitionChunkOffset) -> eyre::Result<bool> {
    let Some(hash) = v1_hashes_at(env, offset)?.and_then(|row| row.tx_path_hash) else {
        return Ok(false);
    };
    if offset.0 == 0 {
        return Ok(true);
    }
    let prev = v1_hashes_at(env, PartitionChunkOffset(offset.0 - 1))?;
    Ok(prev.and_then(|row| row.tx_path_hash) != Some(hash))
}

/// A rocks placement start is a v1 run start, or an info cut inside that same run.
fn v1_placement_start_ok(
    env: &DatabaseEnv,
    start: PartitionChunkOffset,
    data_root: H256,
) -> eyre::Result<bool> {
    if v1_interval_starts_at(env, start)? {
        return Ok(true);
    }
    if start.0 == 0 {
        return Ok(false);
    }
    let hash = v1_hashes_at(env, start)?.and_then(|row| row.tx_path_hash);
    let prev =
        v1_hashes_at(env, PartitionChunkOffset(start.0 - 1))?.and_then(|row| row.tx_path_hash);
    if hash.is_none() || hash != prev {
        return Ok(false);
    }
    let Ok(at) = i32::try_from(start.0) else {
        return Ok(false);
    };
    Ok(v1_infos(env, data_root)?
        .iter()
        .any(|info| info.start_offset.0 == at))
}

fn verify_mdbx_v1_against_rocks(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    db: &DatabaseEnv,
    map: &PlacementMap,
) -> eyre::Result<EngineEquivalenceReport> {
    let mut report = EngineEquivalenceReport::default();
    if v3.schema_bytes()?.as_deref() != Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()) {
        report.schema_mismatch = 1;
        note(&mut report, "schema", "metadata".to_owned());
    }
    if reset_marker_on(env)? != v3.reset_in_progress()? {
        report.reset_marker_mismatch = 1;
        note(&mut report, "reset_marker", "metadata".to_owned());
    }
    let limits = Limits {
        rows: VERIFY_KEYS,
        value_bytes: usize::MAX,
    };
    let mut cache = TxPathBlockCache::new();
    let mut written = FrontierSkips::default();
    let mut frontier = FrontierSkips::default();
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
            &mut report,
        )
    })?;
    report.skipped_orphans = report.skipped_orphans.saturating_add(orphans);
    note_rocks_placements_absent_from_v1(env, v3, &mut report)?;
    compare_v1_pending(env, v3, &written, &frontier, &mut report)?;
    note_extra_rocks_offset_rows(v3, &written, &frontier, &mut report)?;
    verify_content_index(v3, &mut report)?;
    note_index_audit(v3, &mut report)?;
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
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let start = PartitionChunkOffset(run.start);
    let end = PartitionChunkOffset(run.end);
    report.intervals_checked = report.intervals_checked.saturating_add(1);
    report.tx_paths_checked = report.tx_paths_checked.saturating_add(1);
    let Some(stored) = read_v1_tx_path(env, run.tx_hash)? else {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
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
        note(
            report,
            "tx_path",
            format!("offset {} tx header {} missing", start.0, derived.tx_id),
        );
        return Ok(());
    };
    if derived.proof.proof != stored {
        report.tx_paths_mismatches = report.tx_paths_mismatches.saturating_add(1);
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
        let filled = place.tx_start_offset.0 != 0 || place.data_size != 0;
        if filled {
            let base = ledger_base(map.slot_index, map.chunks_per_partition);
            let expected = i64::try_from(seg_derived.tx_ledger_start)
                .ok()
                .zip(base)
                .and_then(|(ledger_start, base)| ledger_start.checked_sub(base));
            let start_ok = expected.and_then(|value| i32::try_from(value).ok())
                == Some(place.tx_start_offset.0);
            if !start_ok || place.data_size != seg_header.data_size {
                report.data_roots_mismatches = report.data_roots_mismatches.saturating_add(1);
                note(report, "data_root", format!("offset {} chain", seg_start.0));
            }
        }
    }
    if let Some(path_end) = written_end
        && !stop_paths
    {
        compare_v1_paths(env, v3, start, path_end, limits, report)?;
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
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut after = start.0.checked_sub(1).map(PartitionChunkOffset);
    loop {
        let batch = read_v1_hash_batch(env, after, limits)?;
        if batch.is_empty() {
            break;
        }
        let mut passed = false;
        for (offset, hashes) in &batch {
            if *offset > end {
                passed = true;
                break;
            }
            let Some(hash) = hashes.data_path_hash else {
                continue;
            };
            let path = read_v1_data_path(env, *offset, hash)?;
            let right = v3.view(|tx| tx.get_data_path_by_offset(*offset))?;
            report.offsets_checked = report.offsets_checked.saturating_add(1);
            if right.as_ref() != Some(&path) {
                report.offset_mismatches = report.offset_mismatches.saturating_add(1);
                report.mismatch_data_path = report.mismatch_data_path.saturating_add(1);
                note(report, "data_path", format!("offset {}", offset.0));
            }
            let meta = v3.view(|tx| tx.get_offset_meta(*offset))?;
            note_mdbx_checksum(true, meta.as_ref(), *offset, report);
            if H256::from(hash_sha256(&path)) != hash {
                report.mismatch_data_path_hash = report.mismatch_data_path_hash.saturating_add(1);
                note(report, "data_path_hash", format!("offset {}", offset.0));
            }
        }
        let last = batch.last().map(|(key, _)| *key);
        if passed || last.is_some_and(|key| key >= end) {
            break;
        }
        after = last;
    }
    Ok(())
}

fn note_rocks_placements_absent_from_v1(
    env: &DatabaseEnv,
    v3: &RocksSubmoduleStore,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut after = None;
    loop {
        let batch = v3.read_placements(after, VERIFY_KEYS)?;
        if batch.is_empty() {
            break;
        }
        for place in &batch {
            if !v1_placement_start_ok(env, place.start, place.data_root)? {
                report.intervals_checked = report.intervals_checked.saturating_add(1);
                report.intervals_mismatches = report.intervals_mismatches.saturating_add(1);
                note(
                    report,
                    "interval",
                    format!("offset {} is only in v3", place.start.0),
                );
            }
        }
        after = batch.last().map(|row| row.start);
    }
    Ok(())
}

fn note_extra_rocks_offset_rows(
    v3: &RocksSubmoduleStore,
    written: &FrontierSkips,
    frontier: &FrontierSkips,
    report: &mut EngineEquivalenceReport,
) -> eyre::Result<()> {
    let mut after = None;
    loop {
        let keys = v3.read_offset_row_keys(after, VERIFY_KEYS)?;
        if keys.is_empty() {
            break;
        }
        for key in &keys {
            if written.covers(key.0) {
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

fn prepare_rocks_target(path: &Path, replace: bool) -> eyre::Result<()> {
    if path.exists() && !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if !path.exists() || dir_is_empty(path)? {
        fs::create_dir_all(path)?;
        write_converting_sentinel(path)?;
        return Ok(());
    }
    refuse_forbidden(path)?;
    if path.join("mdbx.dat").is_file() {
        eyre::bail!(
            "target {} contains mdbx.dat. Refusing to delete an MDBX index or a consensus database.",
            path.display()
        );
    }
    match rocks_schema_text(path)? {
        Some(text) if text == SUBMODULE_SCHEMA_ROCKS_V1 => {
            eyre::bail!(
                "target {} is a completed RocksDB index. Refusing to replace it.",
                path.display()
            );
        }
        Some(_) => {
            eyre::bail!(
                "target {} has an existing index schema. Refusing to delete it.",
                path.display()
            );
        }
        None => {}
    }
    replace_marked_incomplete(path, replace)?;
    Ok(())
}

fn prepare_mdbx_target(path: &Path, args: DatabaseArguments, replace: bool) -> eyre::Result<()> {
    if path.exists() && !path.is_dir() {
        eyre::bail!("target {} is not a directory", path.display());
    }
    if !path.exists() || dir_is_empty(path)? {
        fs::create_dir_all(path)?;
        write_converting_sentinel(path)?;
        return Ok(());
    }
    // `SCHEMA` beside `CONVERTING` is a finished RocksDB index. The sentinel
    // is removed only after that file is durable. Refuse it before any delete.
    match rocks_schema_text(path)? {
        Some(text) if text == SUBMODULE_SCHEMA_ROCKS_V1 => {
            eyre::bail!(
                "target {} is a completed RocksDB index. Refusing to replace it.",
                path.display()
            );
        }
        Some(_) => {
            eyre::bail!(
                "target {} has an existing index schema. Refusing to delete it.",
                path.display()
            );
        }
        None => {}
    }
    refuse_forbidden(path)?;
    if path.join("mdbx.dat").is_file() {
        let args = args.with_exclusive(Some(true));
        let env = match DatabaseEnv::open(path, DatabaseEnvKind::RO, args) {
            Ok(env) => env,
            Err(err) if is_lock_error(&eyre::eyre!("{err}")) => {
                return Err(map_lock(eyre::eyre!("{err}")));
            }
            Err(err) => {
                eyre::bail!(
                    "target {} could not be opened read-only ({err}). Refusing to delete it.",
                    path.display()
                );
            }
        };
        let schema = env.view_eyre(read_schema_bytes)?;
        match schema.as_deref() {
            Some(bytes) if bytes == SUBMODULE_SCHEMA.as_bytes() => {
                eyre::bail!(
                    "target {} is a completed MDBX index. Refusing to replace it.",
                    path.display()
                );
            }
            Some(bytes) if bytes == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() => {}
            Some(_) => {
                eyre::bail!(
                    "target {} schema row is not an incomplete submodule conversion. Refusing to delete a consensus database or other index.",
                    path.display()
                );
            }
            None if mdbx_has_v1_rows(&env)? => {
                eyre::bail!(
                    "target {} is a schema v1 index. Refusing to delete it.",
                    path.display()
                );
            }
            None => {}
        }
        drop(env);
    }
    replace_marked_incomplete(path, replace)?;
    Ok(())
}

fn replace_marked_incomplete(path: &Path, replace: bool) -> eyre::Result<()> {
    if !converting_sentinel_present(path)? {
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
    // Unlock before delete. A missing LOCK file means nothing holds it.
    drop(rocks::hold_rocks_lock(path)?);
    fs::remove_dir_all(path).wrap_err("remove incomplete target")?;
    fs::create_dir_all(path)?;
    write_converting_sentinel(path)?;
    Ok(())
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

fn converting_sentinel_present(path: &Path) -> eyre::Result<bool> {
    let file_path = path.join(CONVERTING_FILE);
    if !file_path.is_file() {
        return Ok(false);
    }
    let bytes = fs::read(&file_path).wrap_err("read CONVERTING")?;
    Ok(bytes == SUBMODULE_CONVERT_INCOMPLETE.as_bytes())
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
    if v3.reset_in_progress()? != source_reset {
        report.reset_marker_mismatch = 1;
        note(&mut report, "reset_marker", "metadata".to_owned());
    }
    if v3.schema_bytes()?.as_deref() != Some(SUBMODULE_SCHEMA_ROCKS_V1.as_bytes()) {
        report.schema_mismatch = 1;
        note(&mut report, "schema", "metadata".to_owned());
    }
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

    use super::super::store::{MdbxSubmoduleStore, SubmoduleStore};
    use super::super::tables::{
        ChunkPathHashes, DataRootInfo, DataRootInfos, PackedOffsetRow, PendingBodyMigration,
        SUBMODULE_SCHEMA_ROCKS_V1, TxLeafBinding,
    };
    use super::{
        ConvertOptions, convert_mdbx_to_rocks, convert_rocks_to_mdbx, verify_mdbx_and_rocks,
        verify_rocks_records,
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
        let root = temp("convert-round");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let back = root.path().join("db-back");
        let options = options_for(&consensus)?;

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
        assert!(report.mismatch_tx_path_hash >= 1, "{report}");
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
        let root = temp("convert-refuse-delete");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let options = options_for(&consensus)?;

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
        let root = temp("convert-dry");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut dry = options_for(&consensus)?;
        dry.dry_run = true;
        let preview = convert_mdbx_to_rocks(&mdbx, &rocks, &dry)?;
        assert!(!rocks.exists());
        assert_copy_counts(&preview);
        let written = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
        assert_eq!(preview.intervals, written.intervals);
        assert_eq!(preview.data_paths, written.data_paths);
        assert_eq!(preview.pending, written.pending);
        assert_eq!(preview.tx_leaf_bindings, written.tx_leaf_bindings);
        assert!(rocks.join("SCHEMA").is_file());
        Ok(())
    }

    #[test]
    fn reset_marker_writes_an_empty_index() -> eyre::Result<()> {
        let root = temp("convert-reset");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let back = root.path().join("db-back");
        {
            let store = MdbxSubmoduleStore::open(&mdbx, testing_args()?)?;
            store.set_reset_in_progress()?;
        }
        let mut dry = options_for(&consensus)?;
        dry.dry_run = true;
        let preview = convert_mdbx_to_rocks(&mdbx, &rocks, &dry)?;
        assert!(!rocks.exists());
        assert!(preview.reset_marker);
        assert_eq!(preview.intervals, 0);
        assert_eq!(preview.data_paths, 0);

        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
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

        let back_stats = convert_rocks_to_mdbx(&rocks, &back, &options_for(&consensus)?)?;
        assert!(back_stats.reset_marker);
        assert_eq!(back_stats.intervals, 0);
        let restored = MdbxSubmoduleStore::open(&back, testing_args()?)?;
        assert!(restored.reset_in_progress()?);
        restored.view(|tx| {
            assert!(tx.get_tx_path_by_offset(PartitionChunkOffset(0))?.is_none());
            Ok(())
        })?;
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
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
        let root = temp("convert-skip");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        add_orphan_and_frontier(&mdbx)?;

        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("does not cover"), "{text}");
        assert!(text.contains("frontier is 13"), "{text}");
        assert!(
            !rocks.exists(),
            "a behind consensus database must not create the target"
        );

        let mut dry = options_for(&consensus)?;
        dry.dry_run = true;
        let err = convert_mdbx_to_rocks(&mdbx, &rocks, &dry).unwrap_err();
        assert!(format!("{err:#}").contains("does not cover"), "{err:#}");
        assert!(!rocks.exists());

        let mut allowed = options_for(&consensus)?;
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
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
        assert!(report.has_mismatch(), "{report}");
        assert!(report.skipped_orphans >= 1, "{report}");
        assert!(report.skipped_frontier >= 1, "{report}");
        Ok(())
    }

    #[test]
    fn empty_consensus_block_index_is_refused() -> eyre::Result<()> {
        let root = temp("convert-empty-consensus");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
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

        convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
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
        let root = temp("convert-missing-place");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        convert_mdbx_to_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
        let clean = verify_mdbx_and_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
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
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options_for(&consensus)?)?;
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

    /// One-chunk v1 placements, one tx each, in a single block. This is the
    /// shape that used to commit twice per placement.
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

        let consensus = root.join("consensus");
        fs::create_dir_all(&consensus)?;
        let txs: Vec<_> = (0..n).map(|i| tx_at(i + 1, 32)).collect();
        let blocks = [block_with(0, &[], 0), block_with(1, &txs, n)];
        let db = open_or_create_db(&consensus, IrysTables::ALL, testing_args()?)?;
        db.update_eyre(|tx| {
            for header in &txs {
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
        let mut cache = TxPathBlockCache::new();
        let mdbx = root.join("db");
        let index = open_or_create_db(&mdbx, master::MasterV1Tables::ALL, testing_args()?)?;
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

    #[test]
    #[ignore = "throughput sample for the v1 convert path"]
    fn measure_v1_one_chunk_convert() -> eyre::Result<()> {
        const N: u64 = 2048;
        let root = temp("convert-v1-rate");
        let consensus = write_v1_one_chunk_partition(root.path(), N)?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let mut options = options_for(&consensus)?;
        options.slot_index = 0;
        options.chunks_per_partition = N;
        let stats = convert_mdbx_to_rocks(&mdbx, &rocks, &options)?;
        assert_eq!(stats.intervals, N);
        assert_eq!(stats.data_paths, N);
        assert_eq!(stats.skipped_frontier, 0);
        let per_s = (N as f64) * 1000.0 / (stats.elapsed_ms.max(1) as f64);
        eprintln!(
            "v1_convert placements={N} elapsed_ms={} placements_per_s={per_s:.1}",
            stats.elapsed_ms
        );
        let report = verify_mdbx_and_rocks(&mdbx, &rocks, &options)?;
        assert!(!report.has_mismatch(), "{report}");
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
    fn verify_rocks_counts_one_byte_in_each_family() -> eyre::Result<()> {
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

        for family in INDEX_FAMILIES {
            refresh_from(&left, &right)?;
            flip_one_stored_byte(&right, family, None)?;
            let report = verify_rocks_records(&left, &right)?;
            assert_eq!(family_mismatches(&report, family), 1, "{family}: {report}");
            assert!(report.has_mismatch(), "{family}: {report}");
            for other in INDEX_FAMILIES {
                if other != family {
                    assert_eq!(
                        family_mismatches(&report, other),
                        0,
                        "{family} also changed {other}: {report}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn verify_rocks_counts_one_byte_in_each_identity_key() -> eyre::Result<()> {
        use crate::metadata::MetadataKey;
        use reth_db::table::Encode as _;

        let root = temp("verify-rocks-identity");
        let left = root.path().join("left");
        let right = root.path().join("right");
        seed_rocks_families(&left)?;
        let keys = [
            MetadataKey::DBSchemaVersion.encode(),
            MetadataKey::PartitionHash.encode(),
            MetadataKey::PackingRecord.encode(),
            EXTRA_METADATA_KEY,
        ];
        for key in keys {
            refresh_from(&left, &right)?;
            flip_one_stored_byte(&right, "Metadata", Some(&key))?;
            let report = verify_rocks_records(&left, &right)?;
            assert_eq!(family_mismatches(&report, "Metadata"), 1, "{report}");
            assert!(report.has_mismatch(), "{report}");
            for name in INDEX_FAMILIES {
                if name != "Metadata" {
                    assert_eq!(family_mismatches(&report, name), 0, "{report}");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn verify_flags_an_invented_checksum() -> eyre::Result<()> {
        let root = temp("verify-invented-crc");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;
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

    #[test]
    fn verify_flags_a_checksum_only_row_the_source_lacks() -> eyre::Result<()> {
        let root = temp("verify-crc-only");
        let consensus = populate_mdbx(root.path())?;
        let mdbx = root.path().join("db");
        let rocks = root.path().join("rocks");
        let options = options_for(&consensus)?;
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
