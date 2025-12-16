use crate::{
    error::{ApiError, ApiStatusResponse},
    ApiState,
};
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

/// Handles the HTTP POST request for adding a chunk to the mempool.
/// This function takes in a JSON payload of a `Chunk` type, encapsulates it
/// into a `ChunkIngressMessage` for further processing by the mempool actor,
/// and manages error handling based on the results of message delivery and validation.
pub async fn post_chunk(
    state: web::Data<ApiState>,
    body: Json<UnpackedChunk>,
) -> Result<HttpResponse, ApiError> {
    let chunk = body.into_inner();
    let data_root = chunk.data_root;
    let number = chunk.tx_offset;
    info!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Received chunk {number} for data root {data_root:?}");

    // Create a message and send it
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestChunk(chunk, oneshot_tx);

    // Handle failure to deliver the message (e.g., channel closed)
    if let Err(err) = state.mempool_service.send(tx_ingress_msg.into()) {
        tracing::error!("Failed to send to mempool channel: {:?}", err);
        return Err((
            format!("Failed to send to mempool channel: {err:?}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into());
    }

    // Handle errors in reading the oneshot response
    let msg_result = match oneshot_rx.await {
        Err(err) => {
            tracing::error!("API: Errors reading the mempool oneshot response {:?}", err);
            return Err((
                format!("Internal error: {err:?}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
                .into());
        }
        Ok(v) => v,
    };

    // If we received a response, check for validation errors within the response
    let inner_result: Result<(), ChunkIngressError> = msg_result;
    if let Err(err) = inner_result {
        warn!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Error processing chunk {number} for data root {data_root:?}: {:?}", &err);
        return match err {
            ChunkIngressError::Critical(err) => match err {
                CriticalChunkIngressError::InvalidProof => {
                    Err((format!("Invalid proof: {err:?}"), StatusCode::BAD_REQUEST).into())
                }
                CriticalChunkIngressError::InvalidDataHash => Err((
                    format!("Invalid data_hash: {err:?}"),
                    StatusCode::BAD_REQUEST,
                )
                    .into()),
                CriticalChunkIngressError::InvalidChunkSize => Err((
                    format!("Invalid chunk size: {err:?}"),
                    StatusCode::BAD_REQUEST,
                )
                    .into()),
                CriticalChunkIngressError::InvalidDataSize => Err((
                    format!("Invalid data_size field: {err:?}"),
                    StatusCode::BAD_REQUEST,
                )
                    .into()),
                CriticalChunkIngressError::InvalidOffset(ref msg) => {
                    Err((format!("Invalid tx_offset: {msg}"), StatusCode::BAD_REQUEST).into())
                }
                CriticalChunkIngressError::ServiceUninitialized => Err((
                    format!("Internal service error: {err:?}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into()),
                CriticalChunkIngressError::DatabaseError => Err((
                    format!("Failed to store chunk: {err:?}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into()),
                CriticalChunkIngressError::Other(err) => Err((
                    format!("Internal error: {err:?}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into()),
            },
            ChunkIngressError::Advisory(err) => {
                let msg = match err {
                    AdvisoryChunkIngressError::PreHeaderOversizedBytes => {
                        format!("Pre-header chunk oversized bytes: {err:?}")
                    }
                    AdvisoryChunkIngressError::PreHeaderOversizedDataPath => {
                        format!("Pre-header chunk oversized data_path: {err:?}")
                    }
                    AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap => {
                        format!("Pre-header chunk tx_offset exceeds cap: {err:?}")
                    }
                    AdvisoryChunkIngressError::PreHeaderInvalidOffset(ref msg) => {
                        format!("Pre-header chunk invalid tx_offset: {msg}")
                    }
                    AdvisoryChunkIngressError::Other(ref msg) => {
                        format!("Internal advisory error: {msg:?}")
                    }
                };
                Ok(ApiStatusResponse(msg, StatusCode::OK).into())
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .finish())
}
