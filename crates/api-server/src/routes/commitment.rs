use crate::{error::ApiError, ApiState};
use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_types::{CommitmentTransaction, SendTraced as _, UnixTimestamp, VersionDiscriminant as _};

pub async fn post_commitment_tx(
    state: web::Data<ApiState>,
    body: Json<CommitmentTransaction>,
) -> Result<HttpResponse, ApiError> {
    let tx = body.into_inner();

    let now = UnixTimestamp::now()
        .map_err(|_| ApiError::from(("System time error", StatusCode::INTERNAL_SERVER_ERROR)))?;

    let hardforks = &state.config.consensus.hardforks;
    if !hardforks.is_commitment_version_valid(tx.version(), now) {
        let minimum = hardforks
            .minimum_commitment_version_at(now)
            .ok_or_else(|| {
                ApiError::from((
                    "Internal configuration error",
                    StatusCode::INTERNAL_SERVER_ERROR,
                ))
            })?;
        return Err(ApiError::InvalidTransactionVersion {
            version: tx.version(),
            minimum,
        });
    }

    process_commitment_transaction(tx, state).await
}

async fn process_commitment_transaction(
    tx: CommitmentTransaction,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestCommitmentTxFromApi(tx, oneshot_tx);

    if let Err(err) = state.mempool_service.send_traced(tx_ingress_msg) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::CommitmentTxIngressMessage: {}",
            err
        );

        return Err(ApiError::from((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )));
    }

    let msg_result = oneshot_rx.await;

    if let Err(err) = msg_result {
        tracing::error!("API: {}", err);
        return Err(ApiError::from((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )));
    }

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
            TxIngressError::Skipped => Ok(HttpResponse::Ok().body(err.to_string())),
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
            TxIngressError::InvalidVersion { version, minimum } => {
                // This path is reached when API check passes but mempool check fails,
                // which can happen if hardfork activation occurs between the two checks
                tracing::debug!(
                    "Version validation at mempool level (race during hardfork activation): version={}, minimum={}",
                    version,
                    minimum
                );
                Err(ApiError::InvalidTransactionVersion { version, minimum })
            }
            TxIngressError::UpdateRewardAddressNotAllowed => {
                Err(ApiError::from((err.to_string(), StatusCode::BAD_REQUEST)))
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
