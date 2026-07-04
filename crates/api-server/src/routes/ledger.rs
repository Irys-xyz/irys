use crate::ApiState;
use crate::error::ApiError;
use crate::routes::parse_ledger_id;
use actix_web::web::{Data, Json, Path};
use awc::http::StatusCode;
use irys_database::{block_header_by_hash, database, db::IrysDatabaseExt as _};
use irys_domain::{BlockBounds, BlockBoundsError, BlockIndex};
use irys_types::{
    DataLedger, DataTransactionHeader, DataTransactionLedger, H256, IrysAddress, IrysBlockHeader,
    LedgerChunkOffset, partition::PartitionAssignment, serialization::string_u64,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LedgerSummary {
    miner_address: String,
    ledger_type: DataLedger,
    assignment_count: usize,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PartitionAssignmentsResponse {
    miner_address: String,
    assignments: Vec<PartitionAssignment>,
    #[serde(with = "string_u64")]
    epoch_height: u64,
    assignment_status: AssignmentStatus,
    hash_analysis: HashAnalysis,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub enum AssignmentStatus {
    FullyAssigned,
    PartiallyAssigned { assigned: usize, total: usize },
    Unassigned,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HashAnalysis {
    total_hashes: usize,
    unique_hashes: usize,
    zero_hashes: usize,
    duplicate_hashes: Vec<String>,
}

fn get_canonical_epoch_snapshot(
    app_state: &Data<ApiState>,
) -> std::sync::Arc<irys_domain::EpochSnapshot> {
    app_state.block_tree.read().canonical_epoch_snapshot()
}

fn analyze_partition_hashes(assignments: &[PartitionAssignment]) -> HashAnalysis {
    let total_hashes = assignments.len();
    let mut hash_counts: HashMap<H256, usize> = HashMap::new();
    let mut zero_hashes = 0;

    for assignment in assignments {
        if assignment.partition_hash == H256::zero() {
            zero_hashes += 1;
        }
        *hash_counts.entry(assignment.partition_hash).or_insert(0) += 1;
    }

    let unique_hashes = hash_counts.len();
    let duplicate_hashes: Vec<String> = hash_counts
        .iter()
        .filter(|&(_, &count)| count > 1)
        .map(|(hash, count)| format!("{hash:?} ({count}x)"))
        .collect();

    HashAnalysis {
        total_hashes,
        unique_hashes,
        zero_hashes,
        duplicate_hashes,
    }
}

fn determine_assignment_status(
    node_assignments: &[PartitionAssignment],
    _epoch_snapshot: &irys_domain::EpochSnapshot,
    _node_address: irys_types::IrysAddress,
) -> AssignmentStatus {
    if node_assignments.is_empty() {
        return AssignmentStatus::Unassigned;
    }

    // Check if all assigned hashes are non-zero and valid
    let assigned_count = node_assignments
        .iter()
        .filter(|a| a.partition_hash != H256::zero())
        .count();

    if assigned_count == node_assignments.len() && assigned_count > 0 {
        AssignmentStatus::FullyAssigned
    } else if assigned_count > 0 {
        AssignmentStatus::PartiallyAssigned {
            assigned: assigned_count,
            total: node_assignments.len(),
        }
    } else {
        AssignmentStatus::Unassigned
    }
}

fn count_assignments_by_ledger_type(
    miner_address: IrysAddress,
    partition_assignments: &[PartitionAssignment],
    ledger_type: DataLedger,
) -> Result<usize, ApiError> {
    let count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
        .count();

    if count == 0 {
        return Err(ApiError::LedgerNotFound {
            miner_address,
            ledger_type,
        });
    }

    Ok(count)
}

fn filter_assignments_by_ledger_type(
    miner_address: IrysAddress,
    partition_assignments: Vec<PartitionAssignment>,
    ledger_type: DataLedger,
) -> Result<Vec<PartitionAssignment>, ApiError> {
    let filtered: Vec<PartitionAssignment> = partition_assignments
        .into_iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
        .collect();

    if filtered.is_empty() {
        return Err(ApiError::LedgerNotFound {
            miner_address,
            ledger_type,
        });
    }

    Ok(filtered)
}

#[expect(clippy::unused_async)]
async fn get_ledger_summary(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<LedgerSummary>, ApiError> {
    let partition_assignments = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        epoch_snapshot.get_partition_assignments(*miner_address)
    };

    let assignment_count =
        count_assignments_by_ledger_type(*miner_address, &partition_assignments, ledger_type)?;

    Ok(Json(LedgerSummary {
        miner_address: miner_address.to_string(),
        ledger_type,
        assignment_count,
    }))
}

pub async fn get_submit_summary(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(miner_address, app_state, DataLedger::Submit).await
}

pub async fn get_publish_summary(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(miner_address, app_state, DataLedger::Publish).await
}

#[expect(clippy::unused_async)]
async fn get_partition_assignments(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    let (filtered_assignments, epoch_height) = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        let all_assignments = epoch_snapshot.get_partition_assignments(*miner_address);
        let filtered =
            filter_assignments_by_ledger_type(*miner_address, all_assignments, ledger_type)?;
        (filtered, epoch_snapshot.epoch_height)
    };

    let assignment_status = determine_assignment_status(
        &filtered_assignments,
        &get_canonical_epoch_snapshot(&app_state),
        *miner_address,
    );
    let hash_analysis = analyze_partition_hashes(&filtered_assignments);

    Ok(Json(PartitionAssignmentsResponse {
        miner_address: miner_address.to_string(),
        assignments: filtered_assignments,
        epoch_height,
        assignment_status,
        hash_analysis,
    }))
}

pub async fn get_submit_assignments(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    get_partition_assignments(miner_address, app_state, DataLedger::Submit).await
}

pub async fn get_publish_assignments(
    miner_address: Path<IrysAddress>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    get_partition_assignments(miner_address, app_state, DataLedger::Publish).await
}

pub async fn get_all_assignments(
    state: Data<ApiState>,
    miner_address: Path<IrysAddress>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    let (partition_assignments, epoch_height) = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&state);
        let assignments = epoch_snapshot.get_partition_assignments(*miner_address);
        (assignments, epoch_snapshot.epoch_height)
    };

    let assignment_status = determine_assignment_status(
        &partition_assignments,
        &get_canonical_epoch_snapshot(&state),
        *miner_address,
    );
    let hash_analysis = analyze_partition_hashes(&partition_assignments);

    Ok(Json(PartitionAssignmentsResponse {
        miner_address: miner_address.to_string(),
        assignments: partition_assignments,
        epoch_height,
        assignment_status,
        hash_analysis,
    }))
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EpochInfoResponse {
    #[serde(with = "string_u64")]
    current_epoch: u64,
    #[serde(with = "string_u64")]
    epoch_block_height: u64,
    total_active_partitions: usize,
    unassigned_partitions: usize,
}

pub async fn get_current_epoch(
    app_state: Data<ApiState>,
) -> Result<Json<EpochInfoResponse>, ApiError> {
    let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);

    Ok(Json(EpochInfoResponse {
        current_epoch: epoch_snapshot.epoch_height,
        epoch_block_height: epoch_snapshot.epoch_block.height,
        total_active_partitions: epoch_snapshot.all_active_partitions.len(),
        unassigned_partitions: epoch_snapshot.unassigned_partitions.len(),
    }))
}

// === Ledger offset → transaction attribution ===

/// Response for the `/ledger/{ledger_id}/offset/{ledger_offset}/tx` endpoints.
///
/// Attributes a canonical ledger chunk offset to the data transaction that
/// owns it, using the block index and canonical block headers — chain
/// attribution, not local storage availability.
///
/// `blockEndOffset` and `txEndOffset` are exclusive upper bounds. All u64
/// fields serialize as JSON strings (matching the rest of the API); the u32
/// `txIndex` stays a JSON number.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LedgerOffsetTxResponse {
    pub ledger_id: u32,
    pub ledger: DataLedger,
    #[serde(with = "string_u64")]
    pub ledger_offset: u64,
    #[serde(with = "string_u64")]
    pub slot_index: u64,
    #[serde(with = "string_u64")]
    pub slot_offset: u64,
    #[serde(with = "string_u64")]
    pub block_height: u64,
    pub block_hash: H256,
    /// First chunk offset the block appended to the ledger (inclusive)
    #[serde(with = "string_u64")]
    pub block_start_offset: u64,
    /// One past the last chunk offset the block appended (exclusive)
    #[serde(with = "string_u64")]
    pub block_end_offset: u64,
    pub tx_id: H256,
    pub data_root: H256,
    /// Position of the tx within the block's tx list for this ledger
    /// (a list index, not a chunk offset)
    pub tx_index: u32,
    #[serde(with = "string_u64")]
    pub tx_start_offset: u64,
    /// One past the tx's last chunk offset (exclusive)
    #[serde(with = "string_u64")]
    pub tx_end_offset: u64,
    #[serde(with = "string_u64")]
    pub chunks_in_tx: u64,
}

#[derive(Deserialize)]
pub struct LedgerOffsetPath {
    ledger_id: u32,
    ledger_offset: u64,
}

pub async fn get_tx_by_ledger_offset(
    state: Data<ApiState>,
    path: Path<LedgerOffsetPath>,
) -> Result<Json<LedgerOffsetTxResponse>, ApiError> {
    let ledger = parse_ledger_id(path.ledger_id)?;
    Ok(Json(resolve_tx_by_ledger_offset(
        &state,
        ledger,
        path.ledger_offset,
    )?))
}

#[derive(Deserialize)]
pub struct SlotOffsetPath {
    ledger_id: u32,
    slot_index: u64,
    slot_offset: u64,
}

pub async fn get_tx_by_slot_offset(
    state: Data<ApiState>,
    path: Path<SlotOffsetPath>,
) -> Result<Json<LedgerOffsetTxResponse>, ApiError> {
    let ledger = parse_ledger_id(path.ledger_id)?;
    let ledger_offset = slot_to_ledger_offset(
        path.slot_index,
        path.slot_offset,
        state.config.consensus.num_chunks_in_partition,
    )
    .map_err(|msg| ApiError::CustomWithStatus(msg, StatusCode::BAD_REQUEST))?;
    resolve_tx_by_ledger_offset(&state, ledger, ledger_offset)
        .map(Json)
        .map_err(|err| match err {
            // Restate not-found errors in the client's slot coordinates — the
            // absolute offset in the inner message never appeared in the
            // request, so on its own it can't be correlated with it.
            ApiError::CustomWithStatus(msg, StatusCode::NOT_FOUND) => ApiError::CustomWithStatus(
                format!(
                    "{msg} (requested slot {}, slot offset {})",
                    path.slot_index, path.slot_offset
                ),
                StatusCode::NOT_FOUND,
            ),
            other => other,
        })
}

fn internal(msg: impl Into<String>) -> ApiError {
    ApiError::CustomWithStatus(msg.into(), StatusCode::INTERNAL_SERVER_ERROR)
}

fn not_found(msg: String) -> ApiError {
    ApiError::CustomWithStatus(msg, StatusCode::NOT_FOUND)
}

/// Converts a slot-relative offset into an absolute ledger offset. Every data
/// ledger's slots span `num_chunks_in_partition` chunks — the partitions
/// within a slot are replicas, so the replication factor does not change the
/// slot's data range.
fn slot_to_ledger_offset(
    slot_index: u64,
    slot_offset: u64,
    num_chunks_in_partition: u64,
) -> Result<u64, String> {
    if slot_offset >= num_chunks_in_partition {
        return Err(format!(
            "Slot offset {slot_offset} must be less than the number of chunks in a partition ({num_chunks_in_partition})"
        ));
    }
    slot_index
        .checked_mul(num_chunks_in_partition)
        .and_then(|base| base.checked_add(slot_offset))
        .ok_or_else(|| format!("Slot index {slot_index} exceeds the ledger offset space"))
}

/// Resolves a canonical ledger chunk offset to the data transaction that owns
/// it. Works for any [`DataLedger`], including the Cascade term ledgers
/// (OneYear/ThirtyDay): before activation (or before any data lands) the
/// lookup reports the offset as unallocated, which maps to 404.
///
/// Every read — the bounds binary search, the block header, and the tx-header
/// walk — happens inside one MDBX view transaction, i.e. one consistent
/// snapshot: a concurrent deep-reorg truncation of the block index can't mix
/// states mid-request, and the walk costs one transaction instead of one per
/// tx. (Indexed blocks are migrated, and migration persists the block header
/// and its tx headers atomically with the index entry, so the header/tx
/// lookups below can't miss for in-snapshot index entries.)
fn resolve_tx_by_ledger_offset(
    state: &ApiState,
    ledger: DataLedger,
    ledger_offset: u64,
) -> Result<LedgerOffsetTxResponse, ApiError> {
    let consensus = &state.config.consensus;
    let chunk_size = consensus.chunk_size;
    let num_chunks_in_partition = consensus.num_chunks_in_partition;

    state
        .db
        .view_eyre(|tx| {
            let bounds = match BlockIndex::block_bounds_in_tx(
                tx,
                ledger,
                LedgerChunkOffset::from(ledger_offset),
            ) {
                Ok(bounds) => bounds,
                Err(e) => return Ok(Err(bounds_error_to_api(ledger, ledger_offset, e))),
            };

            let Some(block) = block_header_by_hash(tx, &bounds.block_hash, false)? else {
                return Ok(Err(internal(format!(
                    "Block {} at height {} is in the block index but its header was not found",
                    bounds.block_hash, bounds.height
                ))));
            };

            let ledger_entry = match validate_block_against_index(&bounds, &block) {
                Ok(entry) => entry,
                Err(msg) => return Ok(Err(internal(msg))),
            };

            let attribution = match attribute_tx_at_offset(
                bounds.height,
                (bounds.start_chunk_offset, bounds.end_chunk_offset),
                &ledger_entry.tx_ids.0,
                chunk_size,
                ledger_offset,
                |tx_id| database::tx_header_by_txid(tx, tx_id),
            ) {
                Ok(attribution) => attribution,
                Err(e) => return Ok(Err(internal(e.to_string()))),
            };

            let tx_index = match u32::try_from(attribution.tx_index) {
                Ok(tx_index) => tx_index,
                Err(_) => {
                    return Ok(Err(internal(format!(
                        "tx index {} exceeds u32",
                        attribution.tx_index
                    ))));
                }
            };

            Ok(Ok(LedgerOffsetTxResponse {
                ledger_id: ledger.get_id(),
                ledger,
                ledger_offset,
                slot_index: ledger_offset / num_chunks_in_partition,
                slot_offset: ledger_offset % num_chunks_in_partition,
                block_height: bounds.height,
                block_hash: bounds.block_hash,
                block_start_offset: bounds.start_chunk_offset,
                block_end_offset: bounds.end_chunk_offset,
                tx_id: attribution.header.id,
                data_root: attribution.header.data_root,
                tx_index,
                tx_start_offset: attribution.tx_start_offset,
                tx_end_offset: attribution.tx_end_offset,
                chunks_in_tx: attribution.chunks_in_tx,
            }))
        })
        .map_err(|e| internal(format!("Database read failed: {e}")))?
}

/// Maps the typed bounds-lookup outcomes onto HTTP semantics: "this offset is
/// not (yet) allocated" variants become 404s, real failures become 500s.
fn bounds_error_to_api(ledger: DataLedger, ledger_offset: u64, err: BlockBoundsError) -> ApiError {
    match err {
        BlockBoundsError::IndexEmpty => not_found("Block index is empty".into()),
        BlockBoundsError::LedgerInactive { .. } => not_found(format!(
            "The {ledger:?} ledger has no indexed data (not active yet?)"
        )),
        BlockBoundsError::OffsetBeyondFrontier { frontier, .. } => not_found(format!(
            "Offset {ledger_offset} is beyond the {ledger:?} ledger frontier ({frontier} chunks)"
        )),
        BlockBoundsError::Internal(e) => internal(format!("Block index lookup failed: {e}")),
    }
}

/// Cross-checks the block header loaded for `bounds.block_hash` against the
/// index-derived bounds. Any divergence is a server-side consistency
/// violation (mapped to 500 by the caller). The header's hash needs no check
/// here — it was looked up by `bounds.block_hash`. The cumulative
/// `total_chunks` comparison is the one with real signal: it distinguishes
/// two forks that share the same tx set at this height but diverge in earlier
/// data content.
fn validate_block_against_index<'a>(
    bounds: &BlockBounds,
    block: &'a IrysBlockHeader,
) -> Result<&'a DataTransactionLedger, String> {
    if block.height != bounds.height {
        return Err(format!(
            "Block {} is indexed at height {} but its header says height {}",
            bounds.block_hash, bounds.height, block.height
        ));
    }
    let ledger_entry = block
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == bounds.ledger)
        .ok_or_else(|| {
            format!(
                "Block index attributes {:?} ledger chunks to block {} at height {} but the block has no such ledger entry",
                bounds.ledger, bounds.block_hash, bounds.height
            )
        })?;
    if ledger_entry.tx_root != bounds.tx_root {
        return Err(format!(
            "tx_root mismatch for {:?} ledger at height {}: block index has {} but block header has {}",
            bounds.ledger, bounds.height, bounds.tx_root, ledger_entry.tx_root
        ));
    }
    if ledger_entry.total_chunks != bounds.end_chunk_offset {
        return Err(format!(
            "total_chunks mismatch for {:?} ledger at height {}: block index says the ledger ends at {} but the block header says {}",
            bounds.ledger, bounds.height, bounds.end_chunk_offset, ledger_entry.total_chunks
        ));
    }
    Ok(ledger_entry)
}

/// A transaction located within a block's appended chunk range.
#[derive(Debug)]
struct TxAttribution {
    tx_index: usize,
    header: DataTransactionHeader,
    tx_start_offset: u64,
    tx_end_offset: u64,
    chunks_in_tx: u64,
}

#[derive(Debug, thiserror::Error)]
enum AttributionError {
    #[error("Failed to load tx header {tx_id}: {err}")]
    TxLookup { tx_id: H256, err: String },
    #[error("Tx {tx_id} is listed in the block at height {height} but its header is missing")]
    MissingTxHeader { tx_id: H256, height: u64 },
    #[error(
        "Block at height {height} introduced chunk range [{block_start}, {block_end}) but its txs only cover [{block_start}, {cursor}), leaving offset {ledger_offset} unattributed"
    )]
    RangeNotCovered {
        height: u64,
        block_start: u64,
        block_end: u64,
        cursor: u64,
        ledger_offset: u64,
    },
    #[error(
        "Tx {tx_id} in the block at height {height} extends the tx chunk ranges to {tx_end_offset}, past the block's appended range [{block_start}, {block_end})"
    )]
    RangeOvershoot {
        tx_id: H256,
        height: u64,
        block_start: u64,
        block_end: u64,
        tx_end_offset: u64,
    },
}

/// Walks a block's ledger txs in order, assigning each the chunk range
/// `[cursor, cursor + ceil(data_size / chunk_size))` starting from the block's
/// first appended offset — mirroring how `BlockIndex::push_block` accumulates
/// `total_chunks` — and returns the tx whose range contains `ledger_offset`.
fn attribute_tx_at_offset(
    height: u64,
    (block_start, block_end): (u64, u64),
    tx_ids: &[H256],
    chunk_size: u64,
    ledger_offset: u64,
    mut load_tx_header: impl FnMut(&H256) -> eyre::Result<Option<DataTransactionHeader>>,
) -> Result<TxAttribution, AttributionError> {
    let mut cursor = block_start;
    for (tx_index, tx_id) in tx_ids.iter().enumerate() {
        let header = load_tx_header(tx_id)
            .map_err(|e| AttributionError::TxLookup {
                tx_id: *tx_id,
                err: e.to_string(),
            })?
            .ok_or(AttributionError::MissingTxHeader {
                tx_id: *tx_id,
                height,
            })?;
        let chunks_in_tx = header.data_size.div_ceil(chunk_size);
        let tx_end_offset = cursor.saturating_add(chunks_in_tx);
        // Over-coverage is as much a consistency violation as the
        // under-coverage RangeNotCovered below: the block's txs must
        // reproduce exactly the chunk range the index recorded.
        if tx_end_offset > block_end {
            return Err(AttributionError::RangeOvershoot {
                tx_id: *tx_id,
                height,
                block_start,
                block_end,
                tx_end_offset,
            });
        }
        if (cursor..tx_end_offset).contains(&ledger_offset) {
            return Ok(TxAttribution {
                tx_index,
                header,
                tx_start_offset: cursor,
                tx_end_offset,
                chunks_in_tx,
            });
        }
        cursor = tx_end_offset;
    }
    Err(AttributionError::RangeNotCovered {
        height,
        block_start,
        block_end,
        cursor,
        ledger_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn make_assignment(ledger: DataLedger, partition_hash: H256) -> PartitionAssignment {
        PartitionAssignment {
            partition_hash,
            miner_address: IrysAddress::ZERO,
            ledger_id: Some(ledger.into()),
            slot_index: Some(0),
        }
    }

    fn nonzero_hash(seed: u8) -> H256 {
        H256::from([seed; 32])
    }

    #[rstest]
    #[case(DataLedger::Publish, DataLedger::Publish, 3, Ok(3))]
    #[case(DataLedger::Submit, DataLedger::Submit, 2, Ok(2))]
    #[case(DataLedger::Publish, DataLedger::Submit, 2, Err(()))]
    #[case(DataLedger::Submit, DataLedger::Publish, 2, Err(()))]
    fn count_assignments_by_ledger_type_cases(
        #[case] assignment_ledger: DataLedger,
        #[case] query_ledger: DataLedger,
        #[case] num_assignments: usize,
        #[case] expected: Result<usize, ()>,
    ) {
        let miner = IrysAddress::ZERO;
        let assignments: Vec<PartitionAssignment> = (0..num_assignments)
            .map(|i| {
                make_assignment(
                    assignment_ledger,
                    nonzero_hash(u8::try_from(i + 1).unwrap_or(1)),
                )
            })
            .collect();

        let result = count_assignments_by_ledger_type(miner, &assignments, query_ledger);

        match expected {
            Ok(count) => assert_eq!(result.unwrap(), count),
            Err(()) => assert!(matches!(result, Err(ApiError::LedgerNotFound { .. }))),
        }
    }

    #[test]
    fn count_assignments_empty_slice_returns_err() {
        let result = count_assignments_by_ledger_type(IrysAddress::ZERO, &[], DataLedger::Publish);
        assert!(matches!(
            result,
            Err(ApiError::LedgerNotFound {
                ledger_type: DataLedger::Publish,
                ..
            })
        ));
    }

    #[test]
    fn count_assignments_none_ledger_id_returns_err() {
        let miner = IrysAddress::ZERO;
        let assignments = vec![PartitionAssignment {
            partition_hash: nonzero_hash(1),
            miner_address: miner,
            ledger_id: None,
            slot_index: Some(0),
        }];
        assert!(matches!(
            count_assignments_by_ledger_type(miner, &assignments, DataLedger::Publish),
            Err(ApiError::LedgerNotFound {
                ledger_type: DataLedger::Publish,
                ..
            })
        ));
    }

    #[test]
    fn count_assignments_mixed_ledgers_counts_only_matching() {
        let miner = IrysAddress::ZERO;
        let assignments = vec![
            make_assignment(DataLedger::Publish, nonzero_hash(1)),
            make_assignment(DataLedger::Publish, nonzero_hash(2)),
            make_assignment(DataLedger::Submit, nonzero_hash(3)),
        ];
        assert_eq!(
            count_assignments_by_ledger_type(miner, &assignments, DataLedger::Publish).unwrap(),
            2
        );
        assert_eq!(
            count_assignments_by_ledger_type(miner, &assignments, DataLedger::Submit).unwrap(),
            1
        );
    }

    // === Ledger offset → transaction attribution ===

    const CHUNK_SIZE: u64 = 32;

    fn data_tx(seed: u8, data_size: u64) -> DataTransactionHeader {
        let mut header = DataTransactionHeader::default();
        header.id = H256::from([seed; 32]);
        header.data_root = H256::from([seed.wrapping_add(100); 32]);
        header.data_size = data_size;
        header
    }

    fn tx_ids(headers: &[DataTransactionHeader]) -> Vec<H256> {
        headers.iter().map(|h| h.id).collect()
    }

    fn lookup(
        headers: &[DataTransactionHeader],
    ) -> impl FnMut(&H256) -> eyre::Result<Option<DataTransactionHeader>> + '_ {
        move |tx_id| Ok(headers.iter().find(|h| h.id == *tx_id).cloned())
    }

    #[test]
    fn attribute_single_tx_single_chunk() {
        let txs = vec![data_tx(1, CHUNK_SIZE)];
        let found =
            attribute_tx_at_offset(7, (5, 6), &tx_ids(&txs), CHUNK_SIZE, 5, lookup(&txs)).unwrap();
        assert_eq!(found.tx_index, 0);
        assert_eq!(found.header.id, txs[0].id);
        assert_eq!(found.tx_start_offset, 5);
        assert_eq!(found.tx_end_offset, 6);
        assert_eq!(found.chunks_in_tx, 1);
    }

    #[rstest]
    #[case::first_chunk(10)]
    #[case::middle_chunk(11)]
    #[case::last_chunk(12)]
    fn attribute_single_tx_multiple_chunks(#[case] offset: u64) {
        // 65 bytes → 3 chunks (partial last chunk rounds up)
        let txs = vec![data_tx(1, 2 * CHUNK_SIZE + 1)];
        let found =
            attribute_tx_at_offset(3, (10, 13), &tx_ids(&txs), CHUNK_SIZE, offset, lookup(&txs))
                .unwrap();
        assert_eq!(found.tx_index, 0);
        assert_eq!(found.tx_start_offset, 10);
        assert_eq!(found.tx_end_offset, 13);
        assert_eq!(found.chunks_in_tx, 3);
    }

    #[rstest]
    #[case::first_tx_first_chunk(100, 0, 100, 102)]
    #[case::first_tx_last_chunk(101, 0, 100, 102)]
    #[case::boundary_second_tx_first_chunk(102, 1, 102, 105)]
    #[case::second_tx_last_chunk(104, 1, 102, 105)]
    #[case::third_tx(105, 2, 105, 106)]
    fn attribute_multiple_txs_in_block(
        #[case] offset: u64,
        #[case] expected_index: usize,
        #[case] expected_start: u64,
        #[case] expected_end: u64,
    ) {
        // tx0: 2 chunks [100, 102), tx1: 3 chunks [102, 105), tx2: 1 chunk [105, 106)
        let txs = vec![
            data_tx(1, 2 * CHUNK_SIZE),
            data_tx(2, 3 * CHUNK_SIZE),
            data_tx(3, 1),
        ];
        let found = attribute_tx_at_offset(
            9,
            (100, 106),
            &tx_ids(&txs),
            CHUNK_SIZE,
            offset,
            lookup(&txs),
        )
        .unwrap();
        assert_eq!(found.tx_index, expected_index);
        assert_eq!(found.header.id, txs[expected_index].id);
        assert_eq!(found.tx_start_offset, expected_start);
        assert_eq!(found.tx_end_offset, expected_end);
    }

    #[test]
    fn attribute_zero_size_tx_owns_no_chunks() {
        let txs = vec![data_tx(1, 0), data_tx(2, CHUNK_SIZE)];
        let found =
            attribute_tx_at_offset(4, (50, 51), &tx_ids(&txs), CHUNK_SIZE, 50, lookup(&txs))
                .unwrap();
        assert_eq!(found.tx_index, 1);
        assert_eq!(found.header.id, txs[1].id);
    }

    #[test]
    fn attribute_missing_tx_header() {
        let txs = vec![data_tx(1, CHUNK_SIZE)];
        let missing_id = H256::from([9; 32]);
        let result = attribute_tx_at_offset(4, (0, 1), &[missing_id], CHUNK_SIZE, 0, lookup(&txs));
        assert!(matches!(
            result,
            Err(AttributionError::MissingTxHeader { tx_id, height: 4 }) if tx_id == missing_id
        ));
    }

    #[test]
    fn attribute_tx_lookup_error() {
        let result = attribute_tx_at_offset(4, (0, 1), &[H256::zero()], CHUNK_SIZE, 0, |_| {
            Err(eyre::eyre!("db exploded"))
        });
        assert!(matches!(result, Err(AttributionError::TxLookup { .. })));
    }

    #[test]
    fn attribute_offset_not_covered_by_txs() {
        // Block claims [0, 3) but its single tx only covers [0, 2)
        let txs = vec![data_tx(1, 2 * CHUNK_SIZE)];
        let result = attribute_tx_at_offset(4, (0, 3), &tx_ids(&txs), CHUNK_SIZE, 2, lookup(&txs));
        assert!(matches!(
            result,
            Err(AttributionError::RangeNotCovered {
                cursor: 2,
                ledger_offset: 2,
                ..
            })
        ));
    }

    #[test]
    fn attribute_txs_overshoot_block_range() {
        // Block claims [0, 2) but its single tx spans 3 chunks — a
        // consistency violation even for offsets the tx would cover
        let txs = vec![data_tx(1, 3 * CHUNK_SIZE)];
        let result = attribute_tx_at_offset(4, (0, 2), &tx_ids(&txs), CHUNK_SIZE, 1, lookup(&txs));
        assert!(matches!(
            result,
            Err(AttributionError::RangeOvershoot {
                tx_end_offset: 3,
                block_end: 2,
                ..
            })
        ));
    }

    fn bounds_and_block(ledger: DataLedger) -> (BlockBounds, irys_types::IrysBlockHeader) {
        let tx_root = H256::from([7; 32]);
        let block_hash = H256::from([8; 32]);
        let mut block = irys_types::IrysBlockHeader::new_mock_header();
        block.height = 42;
        block.block_hash = block_hash;
        block.data_ledgers = vec![DataTransactionLedger {
            ledger_id: ledger.get_id(),
            tx_root,
            total_chunks: 20,
            ..Default::default()
        }];
        let bounds = BlockBounds {
            height: 42,
            block_hash,
            ledger,
            start_chunk_offset: 10,
            end_chunk_offset: 20,
            tx_root,
        };
        (bounds, block)
    }

    #[test]
    fn validate_block_against_index_ok() {
        let (bounds, block) = bounds_and_block(DataLedger::Submit);
        let entry = validate_block_against_index(&bounds, &block).unwrap();
        assert_eq!(entry.tx_root, bounds.tx_root);
    }

    #[test]
    fn validate_block_against_index_height_mismatch() {
        let (bounds, mut block) = bounds_and_block(DataLedger::Submit);
        block.height = 43;
        let err = validate_block_against_index(&bounds, &block).unwrap_err();
        assert!(err.contains("height 43"), "unexpected error: {err}");
    }

    #[test]
    fn validate_block_against_index_missing_ledger_entry() {
        // A pre-Cascade block has no OneYear entry; the index claiming it
        // introduced OneYear chunks is a consistency violation.
        let (mut bounds, block) = bounds_and_block(DataLedger::Submit);
        bounds.ledger = DataLedger::OneYear;
        let err = validate_block_against_index(&bounds, &block).unwrap_err();
        assert!(
            err.contains("no such ledger entry"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_block_against_index_tx_root_mismatch() {
        let (mut bounds, block) = bounds_and_block(DataLedger::Submit);
        bounds.tx_root = H256::from([13; 32]);
        let err = validate_block_against_index(&bounds, &block).unwrap_err();
        assert!(err.contains("tx_root mismatch"), "unexpected error: {err}");
    }

    #[test]
    fn validate_block_against_index_total_chunks_mismatch() {
        // Two forks can share the same tx set (same tx_root) at this height
        // while diverging in earlier data content — the cumulative
        // total_chunks comparison is the check that catches it.
        let (mut bounds, block) = bounds_and_block(DataLedger::Submit);
        bounds.end_chunk_offset = 21;
        let err = validate_block_against_index(&bounds, &block).unwrap_err();
        assert!(
            err.contains("total_chunks mismatch"),
            "unexpected error: {err}"
        );
    }

    #[rstest]
    #[case::slot_zero_start(0, 0, 100, Ok(0))]
    #[case::slot_zero_end(0, 99, 100, Ok(99))]
    #[case::slot_one_start(1, 0, 100, Ok(100))]
    #[case::mid_slot(3, 29, 100, Ok(329))]
    #[case::offset_at_partition_size(0, 100, 100, Err(()))]
    #[case::offset_past_partition_size(2, 101, 100, Err(()))]
    #[case::overflow(u64::MAX, 0, 100, Err(()))]
    fn slot_to_ledger_offset_cases(
        #[case] slot_index: u64,
        #[case] slot_offset: u64,
        #[case] num_chunks: u64,
        #[case] expected: Result<u64, ()>,
    ) {
        let result = slot_to_ledger_offset(slot_index, slot_offset, num_chunks);
        match expected {
            Ok(offset) => assert_eq!(result.unwrap(), offset),
            Err(()) => assert!(result.is_err()),
        }
    }

    #[test]
    fn ledger_offset_response_serialization_contract() {
        let response = LedgerOffsetTxResponse {
            ledger_id: 1,
            ledger: DataLedger::Submit,
            ledger_offset: 129,
            slot_index: 0,
            slot_offset: 129,
            block_height: 157_860,
            block_hash: H256::from([1; 32]),
            block_start_offset: 127,
            block_end_offset: 130,
            tx_id: H256::from([2; 32]),
            data_root: H256::from([3; 32]),
            tx_index: 2,
            tx_start_offset: 127,
            tx_end_offset: 130,
            chunks_in_tx: 3,
        };
        let json = serde_json::to_value(&response).unwrap();
        // Every u64 serializes as a string (matching the rest of the API);
        // the u32 ledgerId/txIndex stay JSON numbers
        assert_eq!(json["ledgerId"], 1);
        assert_eq!(json["ledger"], "Submit");
        assert_eq!(json["ledgerOffset"], "129");
        assert_eq!(json["slotIndex"], "0");
        assert_eq!(json["slotOffset"], "129");
        assert_eq!(json["blockHeight"], "157860");
        assert_eq!(json["blockStartOffset"], "127");
        assert_eq!(json["blockEndOffset"], "130");
        assert_eq!(json["txIndex"], 2);
        assert_eq!(json["txStartOffset"], "127");
        assert_eq!(json["txEndOffset"], "130");
        assert_eq!(json["chunksInTx"], "3");
    }
}
