use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_types::{parse_address, partition::PartitionAssignment, DataLedger};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct LedgerSummary {
    node_id: String,
    ledger_type: DataLedger,
    assignment_count: usize,
}

fn get_canonical_epoch_snapshot(
    app_state: &Data<ApiState>,
) -> std::sync::Arc<irys_domain::EpochSnapshot> {
    app_state.block_tree.read().canonical_epoch_snapshot()
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
