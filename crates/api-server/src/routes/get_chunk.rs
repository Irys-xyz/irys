use crate::ApiState;
use actix_web::{
    web::{self},
    HttpResponse,
};

use irys_database::Ledger;
use irys_storage::ChunkType;
use irys_types::LedgerChunkOffset;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ChunkPath {
    ledger_num: u64,
    ledger_offset: u64,
}

pub async fn get_chunk(
    state: web::Data<ApiState>,
    path: web::Path<ChunkPath>,
) -> actix_web::Result<HttpResponse> {
    let ledger = match Ledger::try_from(path.ledger_num) {
        Ok(l) => l,
        Err(e) => {
            return Ok(HttpResponse::BadRequest().body(format!("Invalid ledger number: {}", e)))
        }
    };

    let Some((chunk_data, chunk_type)) = state.chunk_provider.get_chunk(ledger, path.ledger_offset)
    else {
        return Ok(HttpResponse::NotFound().body("Chunk not found"));
    };

    match chunk_type {
        ChunkType::Data => Ok(HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(chunk_data)),
        other => Ok(HttpResponse::NotFound().body(format!("Chunk not found type: {:?}", other))),
    }
}
