use actix_web::{
    http::header::ContentType,
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::{
    AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError, MempoolServiceMessage,
};
use irys_types::UnpackedChunk;
use tracing::{info, warn};

use crate::ApiState;

/// Handles the HTTP POST request for adding a chunk to the mempool.
/// This function takes in a JSON payload of a `Chunk` type, encapsulates it
/// into a `ChunkIngressMessage` for further processing by the mempool actor,
/// and manages error handling based on the results of message delivery and validation.
pub async fn post_chunk(
    state: web::Data<ApiState>,
    body: Json<UnpackedChunk>,
) -> actix_web::Result<HttpResponse> {
    let chunk = body.into_inner();
    let data_root = chunk.data_root;
    let number = chunk.tx_offset;
    info!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Received chunk");

    // Create a message and send it
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestChunk(chunk, oneshot_tx);

    // Handle failure to deliver the message (e.g., channel closed)
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!("Failed to send to mempool channel: {:?}", err);
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to send to mempool channel: {err:?}")));
    }

    // Handle errors in reading the oneshot response
    let msg_result = match oneshot_rx.await {
        Err(err) => {
            tracing::error!("API: Errors reading the mempool oneshot response {:?}", err);
            return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Internal error: {err:?}")));
        }
        Ok(v) => v,
    };

    // If we received a response, check for validation errors within the response
    let inner_result: Result<(), ChunkIngressError> = msg_result;
    if let Err(err) = inner_result {
        warn!(chunk.data_root = %data_root, chunk.tx_offset = ?number, "Error processing chunk: {:?}", &err);
        return match err {
            ChunkIngressError::Critical(err) => match err {
                CriticalChunkIngressError::InvalidProof => {
                    Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                        .body(format!("Invalid proof: {err:?}")))
                }
                CriticalChunkIngressError::InvalidDataHash => {
                    Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                        .body(format!("Invalid data_hash: {err:?}")))
                }
                CriticalChunkIngressError::InvalidChunkSize => {
                    Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                        .body(format!("Invalid chunk size: {err:?}")))
                }
                CriticalChunkIngressError::InvalidDataSize => {
                    Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                        .body(format!("Invalid data_size field : {err:?}")))
                }

                CriticalChunkIngressError::ServiceUninitialized => {
                    Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(format!("Internal service error: {err:?}")))
                }
                CriticalChunkIngressError::DatabaseError => {
                    Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(format!("Failed to store chunk: {err:?}")))
                }
                CriticalChunkIngressError::Other(err) => {
                    Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(format!("Internal error: {err:?}")))
                }
            },
            ChunkIngressError::Advisory(err) => match err {
                AdvisoryChunkIngressError::PreHeaderOversizedBytes => {
                    Ok(HttpResponse::build(StatusCode::OK)
                        .body(format!("Pre-header chunk oversized bytes: {err:?}")))
                }
                AdvisoryChunkIngressError::PreHeaderOversizedDataPath => {
                    Ok(HttpResponse::build(StatusCode::OK)
                        .body(format!("Pre-header chunk oversized data_path: {err:?}")))
                }
                AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap => {
                    Ok(HttpResponse::build(StatusCode::OK)
                        .body(format!("Pre-header chunk tx_offset exceeds cap: {err:?}")))
                }
                AdvisoryChunkIngressError::Other(err) => Ok(
                    HttpResponse::build(StatusCode::OK).body(format!("Internal error: {err:?}"))
                ),
            },
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .finish())
}
