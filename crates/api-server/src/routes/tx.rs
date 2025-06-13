use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse, Result,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_types::{
    u64_stringify, CommitmentTransaction, DataLedger, IrysTransactionHeader,
    IrysTransactionResponse, H256,
};
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
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::IngestDataTx(tx, oneshot_tx);
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!("API: {:?}", err);
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver chunk: {:?}", err)));
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
            TxIngressError::InvalidSignature => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Signature: {:?}", err))),
            TxIngressError::Unfunded => Ok(HttpResponse::build(StatusCode::PAYMENT_REQUIRED)
                .body(format!("Unfunded: {:?}", err))),
            TxIngressError::Skipped => Ok(HttpResponse::Ok()
                .body("Already processed: the transaction was previously handled")),
            TxIngressError::Other(err) => {
                tracing::error!("API: {:?}", err);
                Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(format!("Failed to deliver transaction: {:?}", err)))
            }
            TxIngressError::InvalidAnchor => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Anchor: {:?}", err))),
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

pub async fn get_transaction_api(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IrysTransactionResponse>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx by tx_id: {}", tx_id);
    get_transaction(&state, tx_id).await.map(web::Json)
}
// Helper function to retrieve IrysTransactionHeader from mdbx
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

// Helper function to retrieve CommitmentTransaction from mdbx
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
// it can retrieve both data or commitment txs
// from either mempool or mdbx
pub async fn get_transaction(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<IrysTransactionResponse, ApiError> {
    // try to get data tx from mempool
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::GetDataTxs(vec![tx_id], oneshot_tx);
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::GetDataTxs: {:?}",
            err
        );
    }
    let mempool_response = oneshot_rx.await.expect(
        "to receive IrysTransactionResponse from MempoolServiceMessage::GetDataTxs message",
    );
    let maybe_mempool_tx = mempool_response.first();
    if let Some(result) = maybe_mempool_tx {
        if let Some(tx) = result {
            return Ok(IrysTransactionResponse::Storage(tx.clone()));
        }
    }

    // try to get commitment tx from mempool
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let tx_ingress_msg = MempoolServiceMessage::GetCommitmentTxs {
        commitment_tx_ids: vec![tx_id],
        response: oneshot_tx,
    };
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!(
            "API Failed to deliver MempoolServiceMessage::GetCommitmentTxs: {:?}",
            err
        );
    }
    let mempool_response = oneshot_rx.await.expect(
        "to receive IrysTransactionResponse from MempoolServiceMessage::GetCommitmentTxs message",
    );
    let maybe_mempool_tx = mempool_response.get(&tx_id);
    if let Some(tx) = maybe_mempool_tx {
        return Ok(IrysTransactionResponse::Commitment(tx.clone()));
    }

    // get from database
    get_storage_transaction(state, tx_id)
        .map(IrysTransactionResponse::Storage)
        .or_else(|err| match err {
            ApiError::ErrNoId { .. } => {
                get_commitment_transaction(state, tx_id).map(IrysTransactionResponse::Commitment)
            }
            other => Err(other),
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

// TODO: REMOVE ME ONCE WE HAVE A GATEWAY
/// Returns whether or not a transaction has been promoted
/// by checking if the ingress_proofs field of the tx's header is `Some`,
///  which only occurs when it's been promoted.
pub async fn get_tx_is_promoted(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<bool>, ApiError> {
    let tx_id: H256 = path.into_inner();
    info!("Get tx_is_promoted by tx_id: {}", tx_id);
    let tx_header = get_storage_transaction(&state, tx_id)?;

    Ok(web::Json(tx_header.ingress_proofs.is_some()))
}
