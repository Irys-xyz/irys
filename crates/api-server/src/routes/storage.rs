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

#[derive(Serialize, Deserialize, Debug)]
pub struct ChunkCountsParams {
    ledger: DataLedger,
    slot_index: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ChunkCountsResponse {
    ledger: DataLedger,
    slot_index: usize,
    data_chunks: usize,
    packed_chunks: usize,
}

impl ChunkCountsResponse {
    fn new(params: &ChunkCountsParams, data_chunks: usize, packed_chunks: usize) -> Self {
        Self {
            ledger: params.ledger,
            slot_index: params.slot_index,
            data_chunks,
            packed_chunks,
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

pub async fn get_chunk_counts(
    params: Path<ChunkCountsParams>,
    app_state: Data<ApiState>,
) -> Result<Json<ChunkCountsResponse>, ApiError> {
    let storage_modules = app_state.chunk_provider.storage_modules_guard.read();

    let storage_module = storage_modules.iter().find(|sm| {
        sm.partition_assignment()
            .map(|pa| {
                pa.ledger_id == Some(params.ledger.into())
                    && pa.slot_index == Some(params.slot_index)
            })
            .unwrap_or(false)
    });

    let (data_chunks, packed_chunks) = if let Some(sm) = storage_module {
        let data_intervals = sm.get_intervals(ChunkType::Data);
        let packed_intervals = sm.get_intervals(ChunkType::Entropy);

        let data_count: usize = data_intervals
            .iter()
            .map(|interval| {
                let start: u32 = interval.start().into();
                let end: u32 = interval.end().into();
                (end - start + 1) as usize
            })
            .sum();

        let packed_count: usize = packed_intervals
            .iter()
            .map(|interval| {
                let start: u32 = interval.start().into();
                let end: u32 = interval.end().into();
                (end - start + 1) as usize
            })
            .sum();

        (data_count, packed_count)
    } else {
        (0, 0)
    };

    Ok(Json(ChunkCountsResponse::new(
        &params,
        data_chunks,
        packed_chunks,
    )))
}
