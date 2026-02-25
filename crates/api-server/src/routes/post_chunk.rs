use crate::{
    error::{ApiError, ApiStatusResponse},
    metrics::{record_chunk_error, record_chunk_processing_duration, record_chunk_received},
    ApiState,
};
use actix_web::{
    http::header::ContentType,
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::{chunk_ingress_service::ChunkIngressMessage, ChunkIngressError};
use irys_types::{SendTraced as _, UnpackedChunk};
use irys_utils::ElapsedMs as _;
use std::time::Instant;
use tracing::{info, instrument, warn};

/// Handles the HTTP POST request for adding a chunk to the chunk ingress service.
/// This function takes in a JSON payload of a `Chunk` type, encapsulates it
/// into a `ChunkIngressMessage` for further processing by the chunk ingress service,
/// and manages error handling based on the results of message delivery and validation.
#[instrument(level = "info", skip_all)]
pub async fn post_chunk(
    state: web::Data<ApiState>,
    body: Json<UnpackedChunk>,
) -> Result<HttpResponse, ApiError> {
    let start = Instant::now();

    let chunk = body.into_inner();
    let chunk_size = chunk.bytes.0.len() as u64;
    let data_root = chunk.data_root;
    let number = chunk.tx_offset;

    record_chunk_received(chunk_size);

    info!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Received chunk");

    // Create a message and send it
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = ChunkIngressMessage::IngestChunk(chunk, Some(oneshot_tx));

    // Handle failure to deliver the message (e.g., channel closed)
    if let Err(err) = state.chunk_ingress.send_traced(tx_ingress_msg) {
        tracing::error!("Failed to send to chunk ingress channel: {:?}", err);
        record_chunk_error("channel_error", false);
        return Err((
            format!("Failed to send to chunk ingress channel: {err:?}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into());
    }

    // Handle errors in reading the oneshot response
    let msg_result = match oneshot_rx.await {
        Err(err) => {
            tracing::error!(
                "API: Errors reading the chunk ingress oneshot response {:?}",
                err
            );
            record_chunk_error("channel_error", false);
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
        warn!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Error processing chunk: {:?}", &err);
        record_chunk_error(err.error_type(), err.is_advisory());

        return if err.is_advisory() {
            Ok(ApiStatusResponse(format!("{err:?}"), StatusCode::OK).into())
        } else {
            let status = match err {
                ChunkIngressError::Critical(ref e) => match e {
                    irys_actors::CriticalChunkIngressError::InvalidProof
                    | irys_actors::CriticalChunkIngressError::InvalidDataHash
                    | irys_actors::CriticalChunkIngressError::InvalidChunkSize
                    | irys_actors::CriticalChunkIngressError::InvalidDataSize
                    | irys_actors::CriticalChunkIngressError::InvalidOffset(_) => {
                        StatusCode::BAD_REQUEST
                    }
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                },
                ChunkIngressError::Advisory(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            Err((format!("{err:?}"), status).into())
        };
    }

    // Record processing duration on success
    record_chunk_processing_duration(start.elapsed_ms());

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .finish())
}
