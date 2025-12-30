use std::time::SystemTime;

use crate::{error::ApiError, ApiState};
use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_types::{CommitmentTransaction, UnixTimestamp, VersionDiscriminant};

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

    // Check for Aurora hardfork
    let now_in_seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let aurora = state
        .config
        .consensus
        .hardforks
        .aurora_at(UnixTimestamp::from_secs(now_in_seconds));

    // Enforce minimum version if Aurora hardfork activated
    let version = tx.version();
    if let Some(aurora) = aurora {
        let min_supported_version = aurora.minimum_commitment_tx_version;
        if version < min_supported_version {
            return Err(ApiError::InvalidTransactionVersion {
                version,
                minimum: min_supported_version,
            });
        }
    }

    // Process the transaction based on version
    match tx {
        CommitmentTransaction::V1(tx_v1) => {
            // Do V1-specific validation/processing here if needed
            // ...

            // Convert back to CommitmentTransaction for mempool
            let tx = CommitmentTransaction::V1(tx_v1);
            process_commitment_transaction(tx, state).await
        }
        CommitmentTransaction::V2(tx_v2) => {
            // Do V2-specific validation/processing here if needed
            // ...

            // Convert back to CommitmentTransaction for mempool
            let tx = CommitmentTransaction::V2(tx_v2);
            process_commitment_transaction(tx, state).await
        }
    }
}

async fn process_commitment_transaction(
    tx: CommitmentTransaction,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    // Validate transaction is valid. Check balances etc etc.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestCommitmentTxFromApi(tx, oneshot_tx);

    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::CommitmentTxIngressMessage: {}",
            err
        );

        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Failed to deliver transaction"));
    }

    let msg_result = oneshot_rx.await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        tracing::error!("API: {}", err);
        return Err(ApiError::from((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        tracing::warn!("API: {}", err);
        return match err {
            TxIngressError::InvalidSignature(address) => Err(ApiError::from((
                format!("{err} (address: {address})"),
                StatusCode::BAD_REQUEST,
            ))),
            TxIngressError::Unfunded(_) => Err(ApiError::from((
                err.to_string(),
                StatusCode::PAYMENT_REQUIRED,
            ))),
            TxIngressError::Skipped => Ok(
                // TODO: we should just print the error's existing to_string representation here
                HttpResponse::Ok()
                    .body("Already processed: the transaction was previously handled"),
            ),
            TxIngressError::Other(err) => {
                tracing::error!("API: {}", err);
                // we explicitly don't forward these to the user
                Err(ApiError::from((
                    "Failed to ingest transaction (other)",
                    StatusCode::INTERNAL_SERVER_ERROR,
                )))
            }
            TxIngressError::InvalidAnchor(_) => {
                Err(ApiError::from((err.to_string(), StatusCode::BAD_REQUEST)))
            }
            TxIngressError::DatabaseError(ref db_err) => {
                tracing::error!("API: Database error: {}", db_err);

                Err(ApiError::from((
                    "Internal database error",
                    StatusCode::INTERNAL_SERVER_ERROR,
                )))
            }
            TxIngressError::ServiceUninitialized => {
                tracing::error!("API: {}", err);

                Err(ApiError::from((
                    "Internal service error".to_owned(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )))
            }
            TxIngressError::CommitmentValidationError(_) => {
                Err(ApiError::from((err.to_string(), StatusCode::BAD_REQUEST)))
            }
            TxIngressError::InvalidLedger(_) => {
                Err(ApiError::from((err.to_string(), StatusCode::BAD_REQUEST)))
            }
            TxIngressError::BalanceFetchError { address, reason } => {
                tracing::error!("API: Balance fetch error for {}: {}", address, reason);

                Err(ApiError::from((
                    "Unable to verify balance".to_owned(),
                    StatusCode::SERVICE_UNAVAILABLE,
                )))
            }
            TxIngressError::MempoolFull(reason) => {
                tracing::warn!("API: Mempool at capacity: {}", reason);

                Err(ApiError::from((
                    "Mempool is at capacity. Please try again later.".to_owned(),
                    StatusCode::SERVICE_UNAVAILABLE,
                )))
            }
            TxIngressError::FundMisalignment(reason) => {
                tracing::debug!("Tx has invalid funding params: {}", reason);
                Err(ApiError::from((
                    "Funding for tx is invalid".to_owned(),
                    StatusCode::BAD_REQUEST,
                )))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
