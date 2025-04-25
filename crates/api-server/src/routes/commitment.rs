use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{CommitmentTxIngressMessage, TxIngressError};
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
    let tx_ingress_msg = CommitmentTxIngressMessage(tx);
    let msg_result = state.mempool.send(tx_ingress_msg).await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver transaction: {:?}", err)));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        return match err {
            TxIngressError::InvalidSignature => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Signature: {:?}", err))),
            TxIngressError::Unfunded => Ok(HttpResponse::build(StatusCode::PAYMENT_REQUIRED)
                .body(format!("Unfunded: {:?}", err))),
            TxIngressError::Skipped => Ok(HttpResponse::Ok()
                .body("Already processed: the transaction was previously handled")),
            TxIngressError::Other(err) => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Failed to deliver transaction: {:?}", err)))
            }
            TxIngressError::InvalidAnchor => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Anchor: {:?}", err))),
            TxIngressError::DatabaseError => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal database error: {:?}", err)))
            }
            TxIngressError::ServiceUninitialized => {
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Internal service error: {:?}", err)))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
