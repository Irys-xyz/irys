use crate::{ApiState, error::ApiError};
use actix_web::{
    HttpResponse,
    http::header::ContentType,
    web::{self},
};

use awc::http::StatusCode;
use irys_types::{ChunkFormat, H256};
use serde::Deserialize;
use tracing::debug;

#[derive(Deserialize)]
pub struct LedgerChunkApiPath {
    ledger_id: u32,
    ledger_offset: u64,
}

pub async fn get_chunk_by_ledger_offset(
    state: web::Data<ApiState>,
    path: web::Path<LedgerChunkApiPath>,
) -> Result<HttpResponse, ApiError> {
    let ledger = crate::routes::parse_ledger_id(path.ledger_id)?;

    match state
        .chunk_provider
        .get_chunk_by_ledger_offset(ledger, path.ledger_offset.into())
    {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(ChunkFormat::Packed(chunk))),
        Ok(None) => Err(("Chunk not found", StatusCode::NOT_FOUND).into()),
        Err(e) => {
            debug!(
                "Error retrieving chunk: ledger_id:{} chunk_offset: {} {}",
                path.ledger_id, path.ledger_offset, e
            );
            Err((
                format!("Error retrieving chunk: {e}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
                .into())
        }
    }
}

#[derive(Deserialize)]
pub struct DataRootChunkApiPath {
    ledger_id: u32,
    data_root: H256,
    offset: u32,
}

pub async fn get_chunk_by_data_root_offset(
    state: web::Data<ApiState>,
    path: web::Path<DataRootChunkApiPath>,
) -> Result<HttpResponse, ApiError> {
    let ledger = crate::routes::parse_ledger_id(path.ledger_id)?;

    match state
        .chunk_provider
        .get_chunk_by_data_root(ledger, path.data_root, path.offset.into())
    {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(chunk)),
        Ok(None) => Err(("Chunk not found", StatusCode::NOT_FOUND).into()),
        Err(e) => Err((
            format!("Error retrieving chunk: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into()),
    }
}
