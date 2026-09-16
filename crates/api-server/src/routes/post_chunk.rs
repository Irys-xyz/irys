use crate::{
    ApiState,
    error::ApiError,
    metrics::{record_chunk_error, record_chunk_processing_duration, record_chunk_received},
};
use actix_web::{
    HttpResponse,
    http::header::ContentType,
    web::{self, Json},
};
use awc::http::StatusCode;
use irys_actors::{HttpChunkEnqueueError, enqueue_http_chunk};
use irys_types::UnpackedChunk;
use irys_utils::ElapsedMs as _;
use std::time::{Duration, Instant};
use tracing::{info, instrument};

fn enqueue_error_status(err: &HttpChunkEnqueueError) -> StatusCode {
    match err {
        HttpChunkEnqueueError::WaitersSaturated | HttpChunkEnqueueError::AdmissionTimeout => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        HttpChunkEnqueueError::ChannelClosed => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn enqueue_error_type(err: &HttpChunkEnqueueError) -> &'static str {
    match err {
        HttpChunkEnqueueError::WaitersSaturated => "waiters_saturated",
        HttpChunkEnqueueError::AdmissionTimeout => "admission_timeout",
        HttpChunkEnqueueError::ChannelClosed => "channel_error",
    }
}

/// Handles the HTTP POST request for adding a chunk to the chunk ingress service.
/// Returns 200 once the body is admitted onto the ingress channel, not after
/// merkle validation, MDBX commit, or packing.
#[instrument(level = "info", skip_all)]
pub async fn post_chunk(
    state: web::Data<ApiState>,
    body: Json<UnpackedChunk>,
) -> Result<HttpResponse, ApiError> {
    let start = Instant::now();

    let chunk = body.into_inner();
    let chunk_size = u64::try_from(chunk.bytes.0.len()).unwrap_or(u64::MAX);
    let data_root = chunk.data_root;
    let number = chunk.tx_offset;

    record_chunk_received(chunk_size);

    info!(chunk.data_root = ?data_root, chunk.tx_offset = ?number, "Received chunk");

    let timeout = Duration::from_millis(state.config.mempool.http_chunk_admission_timeout_millis);
    match enqueue_http_chunk(
        chunk,
        &state.chunk_ingress,
        &state.http_chunk_admission,
        &state.http_chunk_waiters,
        timeout,
    )
    .await
    {
        Ok(()) => {
            record_chunk_processing_duration(start.elapsed_ms());
            Ok(HttpResponse::Ok()
                .content_type(ContentType::json())
                .finish())
        }
        Err(err) => {
            let error_type = enqueue_error_type(&err);
            let advisory = !matches!(err, HttpChunkEnqueueError::ChannelClosed);
            record_chunk_error(error_type, advisory);
            let status = enqueue_error_status(&err);
            Err((err.to_string(), status).into())
        }
    }
}

#[cfg(test)]
mod enqueue_status_tests {
    use super::*;
    use irys_actors::HttpChunkEnqueueError;

    #[test]
    fn waiters_and_timeout_are_503_channel_is_500() {
        assert_eq!(
            enqueue_error_status(&HttpChunkEnqueueError::WaitersSaturated),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            enqueue_error_status(&HttpChunkEnqueueError::AdmissionTimeout),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            enqueue_error_status(&HttpChunkEnqueueError::ChannelClosed),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
