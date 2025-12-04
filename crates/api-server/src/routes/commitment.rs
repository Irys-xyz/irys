use crate::{
    error::{ApiError, ApiStatusResponse},
    ApiState,
};
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
) -> Result<HttpResponse, ApiError> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestCommitmentTxFromApi(tx, oneshot_tx);
    if let Err(err) = state.mempool_service.send(tx_ingress_msg.into()) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::CommitmentTxIngressMessage: {}",
            err
        );

        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Failed to deliver transaction"));

        return Err((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into());
    }

    let msg_result = oneshot_rx.await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        tracing::error!("API: {}", err);
        return Err((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into());
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        tracing::warn!("API: {}", err);
        return match err {
            TxIngressError::InvalidSignature(address) => Err((
                format!("{err} (address: {address})"),
                StatusCode::BAD_REQUEST,
            )
                .into()),
            TxIngressError::Unfunded(_) => {
                Err((err.to_string(), StatusCode::PAYMENT_REQUIRED).into())
            }
            TxIngressError::Skipped => Ok(
                // TODO: we should just print the error's existing to_string representation here
                ApiStatusResponse(
                    "Already processed: the transaction was previously handled".to_owned(),
                    StatusCode::OK,
                )
                .into(),
            ),
            TxIngressError::Other(err) => {
                tracing::error!("API: {}", err);
                // we explicitly don't forward these to the user
                Err((
                    "Failed to ingest transaction (other)",
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into())
            }
            TxIngressError::InvalidAnchor(_) => {
                // Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                //     .body(format!("{err} (anchor: {anchor})")))
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::DatabaseError(ref db_err) => {
                tracing::error!("API: Database error: {}", db_err);
                // Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                //     .body("Internal database error"))
                Err(("Internal database error", StatusCode::INTERNAL_SERVER_ERROR).into())
            }
            TxIngressError::ServiceUninitialized => {
                tracing::error!("API: {}", err);
                // Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                //     .body("Internal service error"))
                Err((
                    "Internal service error".to_owned(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into())
            }
            TxIngressError::CommitmentValidationError(_) => {
                // Ok(HttpResponse::build(StatusCode::BAD_REQUEST).body(format!(
                //     "Commitment validation error: {commitment_validation_error}"
                // )))
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::InvalidLedger(_) => {
                // Ok(HttpResponse::build(StatusCode::BAD_REQUEST).body(format!("{err}")))
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::BalanceFetchError { address, reason } => {
                tracing::error!("API: Balance fetch error for {}: {}", address, reason);

                Err((
                    "Unable to verify balance".to_owned(),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                    .into())
            }
            TxIngressError::MempoolFull(reason) => {
                tracing::warn!("API: Mempool at capacity: {}", reason);
                // Ok(HttpResponse::build(StatusCode::SERVICE_UNAVAILABLE)
                //     .body("Mempool is at capacity. Please try again later."))
                Err((
                    "Mempool is at capacity. Please try again later.".to_owned(),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                    .into())
            }
            TxIngressError::FundMisalignment(reason) => {
                tracing::debug!("Tx has invalid funding params: {}", reason);
                // Ok(HttpResponse::build(StatusCode::BAD_REQUEST).body("Funding for tx is invalid"))
                Err((
                    "Funding for tx is invalid".to_owned(),
                    StatusCode::BAD_REQUEST,
                )
                    .into())
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
