use crate::error::{ApiError, ApiStatusResponse};
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse, Result,
};
use awc::http::StatusCode;
use irys_actors::{
    block_discovery::{get_commitment_tx_in_parallel, get_data_tx_in_parallel},
    mempool_service::{MempoolServiceMessage, TxIngressError},
};
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_types::{
    option_u64_stringify, u64_stringify, CommitmentTransaction, DataLedger, DataTransactionHeader,
    IrysTransactionResponse, SendTraced as _, H256,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Handles the HTTP POST request for adding a transaction to the mempool.
/// This function takes in a JSON payload of a `DataTransactionHeader` type,
/// encapsulates it into a `TxIngressMessage` for further processing by the
/// mempool actor, and manages error handling based on the results of message
/// delivery and transaction validation.
pub async fn post_tx(
    state: web::Data<ApiState>,
    body: Json<DataTransactionHeader>,
) -> Result<HttpResponse, ApiError> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestDataTxFromApi(tx, oneshot_tx);
    if let Err(err) = state.mempool_service.send_traced(tx_ingress_msg) {
        tracing::error!("API: {}", err);
        return Err((
            format!("Failed to deliver chunk: {err}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
            .into());
    }
    let msg_result = oneshot_rx.await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        tracing::error!("API: {}", err);
        return Err((
            format!("Failed to deliver transaction: {err}"),
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
                format!("Invalid Signature: {err} (address: {address})"),
                StatusCode::BAD_REQUEST,
            )
                .into()),
            TxIngressError::Unfunded(tx_id) => Err((
                format!("Unfunded: {err} (tx_id: {tx_id})"),
                StatusCode::PAYMENT_REQUIRED,
            )
                .into()),
            TxIngressError::Skipped => Ok(ApiStatusResponse(
                "Already processed: the transaction was previously handled".to_owned(),
                StatusCode::OK,
            )
            .into()),
            TxIngressError::Other(err) => {
                tracing::error!("API: {}", err);
                // we explicitly don't forward these to the user
                Err((
                    format!("Failed to deliver transaction: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into())
            }
            TxIngressError::InvalidAnchor(anchor) => Err((
                format!("Invalid Anchor: {err} (anchor: {anchor})"),
                StatusCode::BAD_REQUEST,
            )
                .into()),
            TxIngressError::DatabaseError(ref db_err) => {
                tracing::error!("API: {}", err);
                Err((
                    format!("Internal database error: {db_err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into())
            }
            TxIngressError::ServiceUninitialized => {
                tracing::error!("API: {}", err);
                Err((
                    format!("Internal service error: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
                    .into())
            }
            TxIngressError::CommitmentValidationError(commitment_validation_error) => Err((
                format!("Commitment validation error: {commitment_validation_error}"),
                StatusCode::BAD_REQUEST,
            )
                .into()),
            TxIngressError::InvalidLedger(_) => {
                Err((format!("Invalid ledger ID: {err}"), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::BalanceFetchError { address, reason } => {
                tracing::error!("API: Balance fetch error for {}: {}", address, reason);
                Err((
                    format!("Unable to verify balance for {address}: {reason}"),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                    .into())
            }
            TxIngressError::MempoolFull(reason) => {
                tracing::warn!("API: Mempool at capacity: {}", reason);
                Err((
                    format!("Mempool is at capacity. Please try again later. {reason}"),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                    .into())
            }
            TxIngressError::FundMisalignment(reason) => {
                tracing::debug!("Tx has invalid funding params: {}", reason);
                Err((
                    format!("Funding for tx is invalid. {reason}"),
                    StatusCode::BAD_REQUEST,
                )
                    .into())
            }
            TxIngressError::InvalidVersion { version, minimum } => {
                Err(ApiError::InvalidTransactionVersion { version, minimum })
            }
            TxIngressError::UpdateRewardAddressNotAllowed => {
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}

pub async fn get_transaction_api(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransactionResponse>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx by tx_id: {}", tx_id);
    get_transaction(&state, tx_id).await.map(web::Json)
}
/// Helper function to retrieve DataTransactionHeader from mdbx
pub fn get_storage_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<DataTransactionHeader, ApiError> {
    let opt = state
        .db
        .view_eyre(|tx| database::tx_header_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up Irys transaction"),
        })?;
    opt.ok_or(ApiError::ErrNoId {
        id: tx_id.to_string(),
        err: String::from("storage tx not found"),
    })
}

// Helper function to retrieve CommitmentTransaction from mdbx
pub fn get_commitment_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<CommitmentTransaction, ApiError> {
    let opt = state
        .db
        .view_eyre(|tx| database::commitment_tx_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up commitment transaction"),
        })?;
    opt.ok_or(ApiError::ErrNoId {
        id: tx_id.to_string(),
        err: String::from("commitment tx not found"),
    })
}

// Combined function to get either type of transaction
// it can retrieve both data or commitment txs
// from either mempool or mdbx
pub async fn get_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransactionResponse, ApiError> {
    let vec = vec![tx_id];
    if let Ok(mut result) =
        get_commitment_tx_in_parallel(&vec, &state.mempool_guard, &state.db).await
    {
        if let Some(tx) = result.pop() {
            return Ok(IrysTransactionResponse::Commitment(tx));
        }
    };

    if let Ok(mut result) =
        get_data_tx_in_parallel(vec.clone(), &state.mempool_guard, &state.db).await
    {
        if let Some(tx) = result.pop() {
            return Ok(IrysTransactionResponse::Storage(tx));
        }
    };

    Err(ApiError::ErrNoId {
        id: tx_id.to_string(),
        err: "id not found".to_string(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TxOffset {
    #[serde(default, with = "u64_stringify")]
    pub data_start_offset: u64,
}

// Modified to work only with storage transactions
pub async fn get_tx_local_start_offset(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<TxOffset>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx data metadata by tx_id: {}", tx_id);

    // Only works for storage transaction header
    // read storage tx from mempool or database
    let tx_header = get_storage_transaction(&state, tx_id)?;
    let ledger = DataLedger::try_from(tx_header.ledger_id).map_err(|_| ApiError::Internal {
        err: String::from("invalid ledger id"),
    })?;

    match state
        .chunk_provider
        .get_ledger_offsets_for_data_root(ledger, tx_header.data_root)
    {
        Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Ok(None) => Err(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: String::from("Transaction data isn't stored by this node"),
        }),
        Ok(Some(offsets)) => {
            let offset = offsets.first().copied().ok_or(ApiError::Internal {
                err: String::from("ChunkProvider error"), // the ChunkProvider should only return a Some if the vec has at least one element
            })?;
            Ok(web::Json(TxOffset {
                data_start_offset: offset,
            }))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromotionStatus {
    #[serde(default, with = "option_u64_stringify")]
    pub promotion_height: Option<u64>,
}

// TODO: REMOVE ME ONCE WE HAVE A GATEWAY
/// Returns whether or not a transaction has been promoted
/// by checking if the ingress_proofs field of the tx's header is `Some`,
///  which only occurs when it's been promoted.
pub async fn get_tx_promotion_status(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<PromotionStatus>, ApiError> {
    let tx_id: H256 = path.into_inner();
    debug!("Get tx_is_promoted by tx_id: {}", tx_id);

    // DB metadata is authoritative; fetch it first
    let db_metadata = state
        .db
        .view_eyre(|db_tx| {
            irys_database::get_data_tx_metadata(db_tx, &tx_id).map_err(|e| eyre::eyre!("{:?}", e))
        })
        .map_err(|e| ApiError::Internal {
            err: format!("Database error: {}", e),
        })?;

    let promotion_height = if let Some(metadata) = db_metadata {
        // DB metadata exists - use it unconditionally
        metadata.promoted_height
    } else {
        // No DB metadata - fall back to mempool metadata
        state
            .mempool_guard
            .get_tx_metadata(&tx_id)
            .await
            .and_then(|m| m.promoted_height())
    };

    Ok(web::Json(PromotionStatus { promotion_height }))
}

/// GET /v1/tx/{tx_id}/status
/// Returns the current status of a transaction
pub async fn get_tx_status(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<irys_types::TransactionStatusResponse>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx status by tx_id: {}", tx_id);

    let current_head_height = {
        // Get current head height from the block tree
        let block_tree = state.block_tree.read();
        let tip_block =
            block_tree
                .get_block(&block_tree.tip)
                .ok_or_else(|| ApiError::Internal {
                    err: "Unable to get block tree tip".to_owned(),
                })?;
        tip_block.height
    };

    // Load metadata from DB (if present) - check both data and commitment transactions metadata
    let db_metadata = state
        .db
        .view_eyre(|db_tx| {
            let data_metadata = irys_database::get_data_tx_metadata(db_tx, &tx_id)
                .map_err(|e| eyre::eyre!("{:?}", e))?;
            let commitment_metadata = irys_database::get_commitment_tx_metadata(db_tx, &tx_id)
                .map_err(|e| eyre::eyre!("{:?}", e))?;
            Ok(irys_actors::db_metadata_to_tx_metadata(
                commitment_metadata,
                data_metadata,
            ))
        })
        .map_err(|e| ApiError::Internal {
            err: format!("Database error: {}", e),
        })?;

    // Compute status (async mempool reads)
    let status = irys_actors::compute_transaction_status(
        db_metadata,
        &tx_id,
        &state.block_index,
        current_head_height,
        &state.mempool_guard,
    )
    .await
    .map_err(|e| ApiError::Internal {
        err: format!("Status computation error: {}", e),
    })?;

    match status {
        Some(status) => Ok(Json(status)),
        None => Err(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: "Transaction not found".to_owned(),
        }),
    }
}
