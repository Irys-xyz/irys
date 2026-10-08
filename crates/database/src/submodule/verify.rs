//! Compare a pristine schema v1 submodule index with a migrated v2 index.
//!
//! v1 answers use the pre-interval readers: the data path is stored under its
//! hash, and the tx path is stored under the hash on the offset row. v2 answers
//! use the public readers the node calls after migration. Unchanged tables are
//! compared row by row. The walk keeps one cursor row at a time.

use std::path::Path;

use irys_types::{H256, PartitionChunkOffset};
use reth_db::cursor::DbCursorRO as _;
use reth_db::mdbx::DatabaseArguments;
use reth_db::mdbx::Error as MdbxError;
use reth_db::table::Table;
use reth_db::transaction::DbTx;
use reth_db::{DatabaseEnv, DatabaseEnvKind, DatabaseError};

use super::db::{get_data_path_by_offset, get_path_hashes_by_offset, get_tx_path_by_offset};
use super::migrate::require_submodule_index_copy;
use super::tables::v1::ChunkDataPathByPathHash;
use super::tables::{
    ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfosByDataRoot,
    PendingBodyMigrationsByOffset, TxLeafBindingByTxPathHash, TxPathByTxPathHash,
};

const MISMATCH_SAMPLES: usize = 20;

/// One read that differed. `at` is the offset or the table key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MismatchSample {
    pub kind: &'static str,
    pub at: String,
}

/// Counts from one streaming compare. `matches` counts offsets whose four
/// read answers agree. Table rows are counted on their own lines.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadEquivalenceReport {
    pub offsets_checked: u64,
    pub matches: u64,
    pub mismatch_data_path: u64,
    pub mismatch_tx_path: u64,
    pub mismatch_data_path_hash: u64,
    pub mismatch_tx_path_hash: u64,
    pub data_root_infos_checked: u64,
    pub data_root_infos_mismatches: u64,
    pub tx_path_rows_checked: u64,
    pub tx_path_rows_mismatches: u64,
    pub tx_leaf_bindings_checked: u64,
    pub tx_leaf_bindings_mismatches: u64,
    pub pending_body_checked: u64,
    pub pending_body_mismatches: u64,
    pub samples: Vec<MismatchSample>,
}

impl ReadEquivalenceReport {
    pub fn has_mismatch(&self) -> bool {
        self.mismatch_data_path > 0
            || self.mismatch_tx_path > 0
            || self.mismatch_data_path_hash > 0
            || self.mismatch_tx_path_hash > 0
            || self.data_root_infos_mismatches > 0
            || self.tx_path_rows_mismatches > 0
            || self.tx_leaf_bindings_mismatches > 0
            || self.pending_body_mismatches > 0
    }
}

impl std::fmt::Display for ReadEquivalenceReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "offsets_checked: {}", self.offsets_checked)?;
        writeln!(f, "matches: {}", self.matches)?;
        writeln!(
            f,
            "offset_mismatches: {}",
            self.offsets_checked.saturating_sub(self.matches)
        )?;
        writeln!(f, "mismatch_data_path: {}", self.mismatch_data_path)?;
        writeln!(f, "mismatch_tx_path: {}", self.mismatch_tx_path)?;
        writeln!(
            f,
            "mismatch_data_path_hash: {}",
            self.mismatch_data_path_hash
        )?;
        writeln!(f, "mismatch_tx_path_hash: {}", self.mismatch_tx_path_hash)?;
        writeln!(
            f,
            "DataRootInfosByDataRoot_checked: {}",
            self.data_root_infos_checked
        )?;
        writeln!(
            f,
            "DataRootInfosByDataRoot_mismatches: {}",
            self.data_root_infos_mismatches
        )?;
        writeln!(
            f,
            "TxPathByTxPathHash_checked: {}",
            self.tx_path_rows_checked
        )?;
        writeln!(
            f,
            "TxPathByTxPathHash_mismatches: {}",
            self.tx_path_rows_mismatches
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
            "PendingBodyMigrationsByOffset_checked: {}",
            self.pending_body_checked
        )?;
        writeln!(
            f,
            "PendingBodyMigrationsByOffset_mismatches: {}",
            self.pending_body_mismatches
        )?;
        for sample in &self.samples {
            writeln!(f, "mismatch: {} {}", sample.kind, sample.at)?;
        }
        Ok(())
    }
}

/// Open a schema v1 copy for reading. The name must contain `copy`.
///
/// The environment is read-only. This function does not create tables and does
/// not take the exclusive lock.
pub fn open_pristine_v1_read_only(
    path: &Path,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    require_submodule_index_copy(path)?;
    if !path.is_dir() {
        eyre::bail!("{} is not a directory", path.display());
    }
    let args = args.with_log_level(None).with_exclusive(Some(false));
    DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .map_err(|err| eyre::eyre!("open read-only {}: {err}", path.display()))
}

/// Compare every offset in either `ChunkPathHashesByOffset`, then the tables
/// migration leaves in place.
///
/// A missing `PendingBodyMigrationsByOffset` (release mainnet v1) is an empty
/// table. The first [`MISMATCH_SAMPLES`] differences are kept.
pub fn verify_submodule_read_equivalence<A, B>(
    v1: &A,
    v2: &B,
) -> eyre::Result<ReadEquivalenceReport>
where
    A: DbTx,
    B: DbTx,
{
    let mut report = ReadEquivalenceReport::default();
    compare_offsets(v1, v2, &mut report)?;
    let (checked, mismatches) = compare_table::<DataRootInfosByDataRoot, _>(
        v1,
        v2,
        "DataRootInfosByDataRoot",
        &mut report,
        |key| format!("{key:x}"),
    )?;
    report.data_root_infos_checked = checked;
    report.data_root_infos_mismatches = mismatches;
    let (checked, mismatches) =
        compare_table::<TxPathByTxPathHash, _>(v1, v2, "TxPathByTxPathHash", &mut report, |key| {
            format!("{key:x}")
        })?;
    report.tx_path_rows_checked = checked;
    report.tx_path_rows_mismatches = mismatches;
    let (checked, mismatches) = compare_table::<TxLeafBindingByTxPathHash, _>(
        v1,
        v2,
        "TxLeafBindingByTxPathHash",
        &mut report,
        |key| format!("{key:x}"),
    )?;
    report.tx_leaf_bindings_checked = checked;
    report.tx_leaf_bindings_mismatches = mismatches;
    let (checked, mismatches) = compare_table::<PendingBodyMigrationsByOffset, _>(
        v1,
        v2,
        "PendingBodyMigrationsByOffset",
        &mut report,
        |key| format!("offset {}", key.0),
    )?;
    report.pending_body_checked = checked;
    report.pending_body_mismatches = mismatches;
    Ok(report)
}

#[derive(PartialEq, Eq)]
struct OffsetRead {
    data_path: Option<Vec<u8>>,
    tx_path: Option<Vec<u8>>,
    data_path_hash: Option<H256>,
    tx_path_hash: Option<H256>,
}

fn compare_offsets<A, B>(v1: &A, v2: &B, report: &mut ReadEquivalenceReport) -> eyre::Result<()>
where
    A: DbTx,
    B: DbTx,
{
    for_each_row::<ChunkPathHashesByOffset, _>(v1, |offset, hashes| {
        compare_one(v1, v2, offset, Some(&hashes), report)
    })?;
    for_each_row::<ChunkPathHashesByOffset, _>(v2, |offset, _hashes| {
        if table_get::<ChunkPathHashesByOffset>(v1, offset)?.is_some() {
            return Ok(());
        }
        compare_one(v1, v2, offset, None, report)
    })?;
    Ok(())
}

fn compare_one<A, B>(
    v1: &A,
    v2: &B,
    offset: PartitionChunkOffset,
    v1_row: Option<&ChunkPathHashes>,
    report: &mut ReadEquivalenceReport,
) -> eyre::Result<()>
where
    A: DbTx,
    B: DbTx,
{
    let left = v1_offset_read(v1, v1_row)?;
    let right = v2_offset_read(v2, offset)?;
    report.offsets_checked += 1;
    let at = format!("offset {}", offset.0);
    let mut same = true;
    if left.data_path != right.data_path {
        same = false;
        report.mismatch_data_path += 1;
        note(report, "data_path", at.clone());
    }
    if left.tx_path != right.tx_path {
        same = false;
        report.mismatch_tx_path += 1;
        note(report, "tx_path", at.clone());
    }
    if left.data_path_hash != right.data_path_hash {
        same = false;
        report.mismatch_data_path_hash += 1;
        note(report, "data_path_hash", at.clone());
    }
    if left.tx_path_hash != right.tx_path_hash {
        same = false;
        report.mismatch_tx_path_hash += 1;
        note(report, "tx_path_hash", at);
    }
    if same {
        report.matches += 1;
    }
    Ok(())
}

/// Master's point reads. `row` is the `ChunkPathHashesByOffset` value.
fn v1_offset_read<T: DbTx>(tx: &T, row: Option<&ChunkPathHashes>) -> eyre::Result<OffsetRead> {
    let data_path_hash = row.and_then(|hashes| hashes.data_path_hash);
    let tx_path_hash = row.and_then(|hashes| hashes.tx_path_hash);
    let data_path = match data_path_hash {
        Some(hash) => tx.get::<ChunkDataPathByPathHash>(hash)?,
        None => None,
    };
    let tx_path = match tx_path_hash {
        Some(hash) => tx.get::<TxPathByTxPathHash>(hash)?,
        None => None,
    };
    Ok(OffsetRead {
        data_path,
        tx_path,
        data_path_hash,
        tx_path_hash,
    })
}

fn v2_offset_read<T: DbTx>(tx: &T, offset: PartitionChunkOffset) -> eyre::Result<OffsetRead> {
    let hashes = get_path_hashes_by_offset(tx, offset)?;
    Ok(OffsetRead {
        data_path: get_data_path_by_offset(tx, offset)?,
        tx_path: get_tx_path_by_offset(tx, offset)?,
        data_path_hash: hashes.as_ref().and_then(|hashes| hashes.data_path_hash),
        tx_path_hash: hashes.as_ref().and_then(|hashes| hashes.tx_path_hash),
    })
}

fn compare_table<T, F>(
    v1: &impl DbTx,
    v2: &impl DbTx,
    kind: &'static str,
    report: &mut ReadEquivalenceReport,
    fmt_key: F,
) -> eyre::Result<(u64, u64)>
where
    T: Table,
    T::Key: Copy + PartialEq,
    T::Value: PartialEq,
    F: Fn(&T::Key) -> String,
{
    let mut checked = 0_u64;
    let mut mismatches = 0_u64;
    for_each_row::<T, _>(v1, |key, value| {
        checked += 1;
        if table_get::<T>(v2, key)?.as_ref() != Some(&value) {
            mismatches += 1;
            note(report, kind, fmt_key(&key));
        }
        Ok(())
    })?;
    for_each_row::<T, _>(v2, |key, _value| {
        if table_get::<T>(v1, key)?.is_some() {
            return Ok(());
        }
        checked += 1;
        mismatches += 1;
        note(report, kind, fmt_key(&key));
        Ok(())
    })?;
    Ok((checked, mismatches))
}

fn for_each_row<T, F>(tx: &impl DbTx, mut row: F) -> eyre::Result<()>
where
    T: Table,
    F: FnMut(T::Key, T::Value) -> eyre::Result<()>,
{
    let mut cursor = match tx.cursor_read::<T>() {
        Ok(cursor) => cursor,
        Err(err) if missing_table(&err) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in cursor.walk(None)? {
        let (key, value) = entry?;
        row(key, value)?;
    }
    Ok(())
}

fn table_get<T: Table>(tx: &impl DbTx, key: T::Key) -> eyre::Result<Option<T::Value>> {
    match tx.get::<T>(key) {
        Ok(value) => Ok(value),
        Err(err) if missing_table(&err) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn note(report: &mut ReadEquivalenceReport, kind: &'static str, at: String) {
    if report.samples.len() < MISMATCH_SAMPLES {
        report.samples.push(MismatchSample { kind, at });
    }
}

fn missing_table(err: &DatabaseError) -> bool {
    let DatabaseError::Open(info) = err else {
        return false;
    };
    *info == MdbxError::NotFound.into()
}
