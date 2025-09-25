use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json, Path};
use irys_domain::ChunkType;
use irys_types::DataLedger;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageIntervalsParams {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChunkInterval {
    start: u32,
    end: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageIntervalsResponse {
    ledger: DataLedger,
    slot_index: usize,
    chunk_type: ChunkType,
    intervals: Vec<ChunkInterval>,
}

impl StorageIntervalsResponse {
    fn new(params: &StorageIntervalsParams, intervals: Vec<ChunkInterval>) -> Self {
        Self {
            ledger: params.ledger,
            slot_index: params.slot_index,
            chunk_type: params.chunk_type,
            intervals,
        }
    }
}

pub async fn get_intervals(
    params: Path<StorageIntervalsParams>,
    app_state: Data<ApiState>,
) -> Result<Json<StorageIntervalsResponse>, ApiError> {
    let storage_modules = app_state.chunk_provider.storage_modules_guard.read();

    let intervals = storage_modules
        .iter()
        .find(|sm| {
            sm.partition_assignment()
                .map(|pa| {
                    pa.ledger_id == Some(params.ledger.into())
                        && pa.slot_index == Some(params.slot_index)
                })
                .unwrap_or(false)
        })
        .map(|sm| {
            sm.get_intervals(params.chunk_type)
                .into_iter()
                .map(|interval| ChunkInterval {
                    start: interval.start().into(),
                    end: interval.end().into(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Json(StorageIntervalsResponse::new(&params, intervals)))
}
