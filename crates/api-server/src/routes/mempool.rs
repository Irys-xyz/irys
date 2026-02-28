use crate::error::ApiError;
use crate::ApiState;
use actix_web::{web, HttpResponse, Result};
use irys_actors::mempool_service::{MempoolServiceMessage, TxReadError};
use irys_types::SendTraced as _;

/// GET /v1/mempool/status
pub async fn get_mempool_status(state: web::Data<ApiState>) -> Result<HttpResponse> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    state
        .mempool_service
        .send_traced(MempoolServiceMessage::GetMempoolStatus(tx))
        .map_err(|_| ApiError::Internal {
            err: "Mempool service unavailable".into(),
        })?;

    match rx.await {
        Ok(Ok(status)) => Ok(HttpResponse::Ok().json(status)),
        Ok(Err(err)) => match err {
            TxReadError::ServiceUninitialized => Err(ApiError::Internal {
                err: "Mempool service not initialized".into(),
            }
            .into()),
            _ => Err(ApiError::Internal {
                err: format!("Failed to get mempool status: {err:?}"),
            }
            .into()),
        },
        Err(_) => Err(ApiError::Internal {
            err: "Failed to receive response from mempool service".into(),
        }
        .into()),
    }
}
