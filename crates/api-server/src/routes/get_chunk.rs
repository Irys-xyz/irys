use crate::ApiState;
use actix_web::{
    web::{self},
    HttpResponse,
};

use irys_database::Ledger;
use irys_types::{ChunkFormat, H256};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct LedgerChunkApiPath {
    ledger_id: u32,
    ledger_offset: u64,
}

pub async fn get_chunk_by_ledger_offset(
    state: web::Data<ApiState>,
    path: web::Path<LedgerChunkApiPath>,
) -> actix_web::Result<HttpResponse> {
    let ledger = match Ledger::try_from(path.ledger_id) {
        Ok(l) => l,
        Err(e) => return Ok(HttpResponse::BadRequest().body(format!("Invalid ledger id: {}", e))),
    };

    match state
        .chunk_provider
        .get_chunk_by_ledger_offset(ledger, path.ledger_offset.into())
    {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok().json(ChunkFormat::Packed(chunk))),
        Ok(None) => Ok(HttpResponse::NotFound().body("Chunk not found")),
        Err(e) => {
            Ok(HttpResponse::InternalServerError().body(format!("Error retrieving chunk: {}", e)))
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
) -> actix_web::Result<HttpResponse> {
    let ledger = match Ledger::try_from(path.ledger_id) {
        Ok(l) => l,
        Err(e) => return Ok(HttpResponse::BadRequest().body(format!("Invalid ledger id: {}", e))),
    };

    match state
        .chunk_provider
        .get_chunk_by_data_root(ledger, path.data_root, path.offset.into())
    {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok().json(chunk)),
        Ok(None) => Ok(HttpResponse::NotFound().body("Chunk not found")),
        Err(e) => {
            Ok(HttpResponse::InternalServerError().body(format!("Error retrieving chunk: {}", e)))
        }
    }
}
