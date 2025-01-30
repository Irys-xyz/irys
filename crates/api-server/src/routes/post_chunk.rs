use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{ChunkIngressError, ChunkIngressMessage};
use irys_types::UnpackedChunk;
use log::info;

use crate::ApiState;

/// Handles the HTTP POST request for adding a chunk to the mempool.
/// This function takes in a JSON payload of a `Chunk` type, encapsulates it
/// into a `ChunkIngressMessage` for further processing by the mempool actor,
/// and manages error handling based on the results of message delivery and validation.
pub async fn post_chunk(
    state: web::Data<ApiState>,
    body: Json<UnpackedChunk>,
) -> actix_web::Result<HttpResponse> {
    info!("Received chunk");
    let chunk = body.into_inner();

    // Create an actor message and send it
    let chunk_ingress_message = ChunkIngressMessage(chunk);
    let msg_result = state.mempool.send(chunk_ingress_message).await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver chunk: {:?}", err)));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        return match err {
            ChunkIngressError::InvalidProof => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid proof: {:?}", err))),
            ChunkIngressError::InvalidDataHash => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid data_hash: {:?}", err))),
            ChunkIngressError::InvalidChunkSize => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid chunk size: {:?}", err))),
            ChunkIngressError::UnknownTransaction => {
                Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                    .body(format!("Unknown transaction: {:?}", err)))
            }
            ChunkIngressError::DatabaseError => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Failed to store chunk: {:?}", err)))
            }
            ChunkIngressError::Other(err) => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal error: {:?}", err)))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
