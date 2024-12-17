use crate::ApiState;
use actix_web::{
    web::{self},
    HttpResponse,
};
use irys_actors::chunk_provider::GetChunkMessage;
use irys_database::Ledger;
use irys_storage::ChunkType;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ChunkPath {
    ledger_num: u64,
    ledger_offset: u64,
}

/// Retrieves a chunk from storage
pub async fn get_chunk(
    state: web::Data<ApiState>,
    path: web::Path<ChunkPath>,
) -> actix_web::Result<HttpResponse> {
    let ledger_num = path.ledger_num;
    let ledger_offset = path.ledger_offset;

    let ledger = match Ledger::try_from(ledger_num) {
        Ok(l) => l,
        Err(e) => {
            return Ok(HttpResponse::BadRequest().body(format!("Invalid ledger number: {}", e)))
        }
    };

    let msg_result = state
        .chunk_provider
        .send(GetChunkMessage {
            ledger,
            offset: ledger_offset,
        })
        .await
        .map_err(|e| {
            HttpResponse::InternalServerError().body(format!("Failed to retrieve chunk: {:?}", e))
        });

    let chunk_info = match msg_result {
        Ok(Some(info)) => info,
        Ok(None) => return Ok(HttpResponse::NotFound().body("Chunk not found")),
        Err(response) => return Ok(response),
    };

    let (chunk_data, chunk_type) = chunk_info;

    match chunk_type {
        ChunkType::Data => Ok(HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(chunk_data)),
        _ => Ok(HttpResponse::NotFound().body(format!("Chunk not found type: {:?}", chunk_type))),
    }
}
