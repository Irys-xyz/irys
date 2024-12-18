use crate::ApiState;
use actix_web::{
    web::{self},
    HttpResponse,
};

use irys_database::Ledger;
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

    match state.chunk_provider.get_chunk(ledger, path.ledger_offset) {
        Ok(Some(chunk)) => Ok(HttpResponse::Ok().json(chunk)),
        Ok(None) => Ok(HttpResponse::NotFound().body("Chunk not found")),
        Err(e) => {
            Ok(HttpResponse::InternalServerError().body(format!("Error retrieving chunk: {}", e)))
        }
    }
}
