use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse, Result,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{TxIngressError, TxIngressMessage};
use irys_database::{database, DataLedger};
use irys_types::{u64_stringify, CommitmentTransaction, IrysTransactionHeader, H256};
use reth_db::Database;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Handles the HTTP POST request for adding a transaction to the mempool.
/// This function takes in a JSON payload of a `IrysTransactionHeader` type,
/// encapsulates it into a `TxIngressMessage` for further processing by the
/// mempool actor, and manages error handling based on the results of message
/// delivery and transaction validation.
pub async fn post_tx(
    state: web::Data<ApiState>,
    body: Json<IrysTransactionHeader>,
) -> actix_web::Result<HttpResponse> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let tx_ingress_msg = TxIngressMessage(tx);
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
                .body(format!("Invalid Signature: {:?}", err))),
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

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransaction {
    #[serde(rename = "commitment")]
    Commitment(CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(IrysTransactionHeader),
}

pub async fn get_transaction_api(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransaction>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx by tx_id: {}", tx_id);
    get_transaction(&state, tx_id).map(web::Json)
}
// Helper function to retrieve IrysTransactionHeader
pub fn get_storage_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransactionHeader, ApiError> {
    state
        .db
        .view_eyre(|tx| database::tx_header_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up Irys transaction"),
        })
        .and_then(|opt| {
            opt.ok_or(ApiError::ErrNoId {
                id: tx_id.to_string(),
                err: String::from("storage tx not found"),
            })
        })
}

// Helper function to retrieve CommitmentTransaction
pub fn get_commitment_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<CommitmentTransaction, ApiError> {
    state
        .db
        .view_eyre(|tx| database::commitment_tx_by_txid(tx, &tx_id))
        .map_err(|_| ApiError::Internal {
            err: String::from("db error while looking up commitment transaction"),
        })
        .and_then(|opt| {
            opt.ok_or(ApiError::ErrNoId {
                id: tx_id.to_string(),
                err: String::from("commitment tx not found"),
            })
        })
}

// Combined function to get either type of transaction
pub fn get_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransaction, ApiError> {
    get_storage_transaction(state, tx_id)
        .map(IrysTransaction::Storage)
        .or_else(|err| match err {
            ApiError::ErrNoId { .. } => {
                get_commitment_transaction(state, tx_id).map(IrysTransaction::Commitment)
            }
            other => Err(other),
        })
}

// Modified to work only with storage transactions
pub async fn get_tx_local_start_offset(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<TxOffset>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx data metadata by tx_id: {}", tx_id);

    // Only works for storage transaction header
    let tx_header = get_storage_transaction(&state, tx_id)?;
    let ledger = DataLedger::try_from(tx_header.ledger_id).unwrap();

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
pub struct TxOffset {
    #[serde(default, with = "u64_stringify")]
    pub data_start_offset: u64,
}
