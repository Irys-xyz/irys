use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_types::CommitmentTransaction;

/// Handles the HTTP POST request for adding a transaction to the mempool.
/// This function takes in a JSON payload of a `CommitmentTransaction` type,
/// encapsulates it into a `CommitmentTxIngressMessage` for further processing by the
/// mempool actor, and manages error handling based on the results of message
/// delivery and transaction validation.
pub async fn post_commitment_tx(
    state: web::Data<ApiState>,
    body: Json<CommitmentTransaction>,
) -> actix_web::Result<HttpResponse> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngressCommitmentTx(tx, oneshot_tx);
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::CommitmentTxIngressMessage: {:?}",
            err
        );
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver transaction: {:?}", err)));
    }

    let msg_result = oneshot_rx.await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        tracing::error!("API: {:?}", err);
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver transaction: {:?}", err)));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        tracing::warn!("API: {:?}", err);
        return match err {
            TxIngressError::InvalidSignature => {
                Ok(HttpResponse::build(StatusCode::BAD_REQUEST).body(format!("{:?}", err)))
            }
            TxIngressError::Unfunded => {
                Ok(HttpResponse::build(StatusCode::PAYMENT_REQUIRED).body(format!("{:?}", err)))
            }
            TxIngressError::Skipped => Ok(HttpResponse::Ok()
                .body("Already processed: the transaction was previously handled")),
            TxIngressError::Other(err) => {
                tracing::error!("API: {:?}", err);
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Failed to deliver transaction: {:?}", err)))
            }
            TxIngressError::InvalidAnchor => {
                Ok(HttpResponse::build(StatusCode::BAD_REQUEST).body(format!("{:?}", err)))
            }
            TxIngressError::DatabaseError => {
                tracing::error!("API: {:?}", err);
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal database error: {:?}", err)))
            }
            TxIngressError::ServiceUninitialized => {
                tracing::error!("API: {:?}", err);
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal service error: {:?}", err)))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
