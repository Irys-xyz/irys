use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_types::{parse_address, partition::PartitionAssignment, DataLedger, H256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug)]
pub struct LedgerSummary {
    node_id: String,
    ledger_type: DataLedger,
    assignment_count: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PartitionAssignmentsResponse {
    node_id: String,
    assignments: Vec<PartitionAssignment>,
    epoch_height: u64,
    assignment_status: AssignmentStatus,
    hash_analysis: HashAnalysis,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AssignmentStatus {
    FullyAssigned,
    PartiallyAssigned { assigned: usize, total: usize },
    Unassigned,
}

#[derive(Serialize, Deserialize, Debug)]
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
        .filter(|(_, &count)| count > 1)
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
    _node_address: irys_types::Address,
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
    node_id: &str,
    partition_assignments: &[PartitionAssignment],
    ledger_type: DataLedger,
) -> Result<usize, ApiError> {
    let count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
        .count();

    if count == 0 {
        return Err(ApiError::LedgerNotFound {
            node_id: node_id.to_string(),
            ledger_type,
        });
    }

    Ok(count)
}

fn filter_assignments_by_ledger_type(
    node_id: &str,
    partition_assignments: Vec<PartitionAssignment>,
    ledger_type: DataLedger,
) -> Result<Vec<PartitionAssignment>, ApiError> {
    let filtered: Vec<PartitionAssignment> = partition_assignments
        .into_iter()
        .filter(|pa| pa.ledger_id == Some(ledger_type.into()))
        .collect();

    if filtered.is_empty() {
        return Err(ApiError::LedgerNotFound {
            node_id: node_id.to_string(),
            ledger_type,
        });
    }

    Ok(filtered)
}

#[expect(clippy::unused_async)]
async fn get_ledger_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<LedgerSummary>, ApiError> {
    let node_address = parse_address(node_id.as_str())?;

    let partition_assignments = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        epoch_snapshot.get_partition_assignments(node_address)
    };

    let assignment_count =
        count_assignments_by_ledger_type(&node_id, &partition_assignments, ledger_type)?;

    Ok(Json(LedgerSummary {
        node_id: node_id.to_string(),
        ledger_type,
        assignment_count,
    }))
}

pub async fn get_submit_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(node_id, app_state, DataLedger::Submit).await
}

pub async fn get_publish_summary(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<LedgerSummary>, ApiError> {
    get_ledger_summary(node_id, app_state, DataLedger::Publish).await
}

#[expect(clippy::unused_async)]
async fn get_partition_assignments(
    node_id: Path<String>,
    app_state: Data<ApiState>,
    ledger_type: DataLedger,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    let node_address = parse_address(node_id.as_str())?;

    let (filtered_assignments, epoch_height) = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        let all_assignments = epoch_snapshot.get_partition_assignments(node_address);
        let filtered = filter_assignments_by_ledger_type(&node_id, all_assignments, ledger_type)?;
        (filtered, epoch_snapshot.epoch_height)
    };

    let assignment_status = determine_assignment_status(
        &filtered_assignments,
        &get_canonical_epoch_snapshot(&app_state),
        node_address,
    );
    let hash_analysis = analyze_partition_hashes(&filtered_assignments);

    Ok(Json(PartitionAssignmentsResponse {
        node_id: node_id.to_string(),
        assignments: filtered_assignments,
        epoch_height,
        assignment_status,
        hash_analysis,
    }))
}

pub async fn get_submit_assignments(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    get_partition_assignments(node_id, app_state, DataLedger::Submit).await
}

pub async fn get_publish_assignments(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    get_partition_assignments(node_id, app_state, DataLedger::Publish).await
}

pub async fn get_all_assignments(
    node_id: Path<String>,
    app_state: Data<ApiState>,
) -> Result<Json<PartitionAssignmentsResponse>, ApiError> {
    let node_address = parse_address(node_id.as_str())?;

    let (partition_assignments, epoch_height) = {
        let epoch_snapshot = get_canonical_epoch_snapshot(&app_state);
        let assignments = epoch_snapshot.get_partition_assignments(node_address);
        (assignments, epoch_snapshot.epoch_height)
    };

    let assignment_status = determine_assignment_status(
        &partition_assignments,
        &get_canonical_epoch_snapshot(&app_state),
        node_address,
    );
    let hash_analysis = analyze_partition_hashes(&partition_assignments);

    Ok(Json(PartitionAssignmentsResponse {
        node_id: node_id.to_string(),
        assignments: partition_assignments,
        epoch_height,
        assignment_status,
        hash_analysis,
    }))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EpochInfoResponse {
    current_epoch: u64,
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
