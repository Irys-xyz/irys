use crate::ApiState;
use crate::error::{ApiError, ApiStatusResponse};
use crate::routes::tx_ingress::{MEMPOOL_REPLY_TIMEOUT, ingest_via_mempool};
use actix_web::{
    HttpResponse, Result,
    web::{self, Json},
};
use awc::http::StatusCode;
use irys_actors::ingress_status::{
    self, IngressProofAssessment, IngressProofSigner, IngressStatusQuery,
};
use irys_actors::{
    block_discovery::{get_commitment_tx_in_parallel, get_data_tx_in_parallel},
    mempool_service::{MempoolServiceMessage, TxIngressError},
};
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_domain::EpochSnapshot;
use irys_types::{
    CommitmentTransaction, DataLedger, DataTransactionHeader, H256, IrysTransactionResponse,
    UnixTimestamp, option_u64_stringify, u64_stringify,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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
    let inner_result = ingest_via_mempool(
        &state.tx_ingress_admission,
        &state.mempool_service,
        MEMPOOL_REPLY_TIMEOUT,
        |reply| MempoolServiceMessage::IngestDataTxFromApi(tx, reply),
    )
    .await?;

    // If message delivery succeeded, check for validation errors within the response
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
            TxIngressError::ZeroDataSize(_) => {
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::PrefixSizeExceedsDataSize(_) => {
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::ChainIdMismatch { .. } => {
                Err((err.to_string(), StatusCode::BAD_REQUEST).into())
            }
            TxIngressError::DataSizeExceedsMax { .. } => {
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
        && let Some(tx) = result.pop()
    {
        return Ok(IrysTransactionResponse::Commitment(tx));
    };

    if let Ok(mut result) =
        get_data_tx_in_parallel(vec.clone(), &state.mempool_guard, &state.db).await
        && let Some(tx) = result.pop()
    {
        return Ok(IrysTransactionResponse::Storage(tx));
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

    // `promoted_height` is written to the DB only at migration, so a
    // confirmed-but-unmigrated promotion lives only in the live mempool. Prefer
    // the DB value when present; otherwise fall back to the mempool (a DB row
    // may exist from the Submit migration with `promoted_height = None` while
    // the Publish block is still unmigrated).
    let promotion_height = match db_metadata.as_ref().and_then(|m| m.promoted_height) {
        Some(height) => Some(height),
        None => match state.mempool_guard.get_tx_metadata(&tx_id).await {
            Some(mempool_meta) => mempool_meta.promoted_height(),
            // No mempool entry: known (DB row exists) but not promoted, or
            // entirely unknown → 404.
            None if db_metadata.is_none() => {
                return Err(ApiError::ErrNoId {
                    id: tx_id.to_string(),
                    err: String::from("Unable to find tx"),
                });
            }
            None => None,
        },
    };

    Ok(web::Json(PromotionStatus { promotion_height }))
}

/// Read-only Submit-ledger ingress status for one data transaction.
///
/// `promotion_height` is the only evidence of promotion. Enough eligible
/// proofs leave it null. `proof_requirements_met` is true only when both
/// deficits are zero, and it does not promise inclusion in the next block.
/// The deficits overlap: do not add `remaining.total` and `remaining.assigned`.
///
/// Once promoted, `proofs`, `requirements`, `remaining`, and
/// `proof_requirements_met` are null. Cached rows are not the historical
/// promotion set, and current configured requirements say nothing about the
/// gate the promotion actually passed.
///
/// While unpromoted, an empty cache is a confirmed empty set: `proofs` is
/// present, every count is zero, and `miners` is empty. A lookup failure is
/// HTTP 500. `requirements.assigned` is then the clamped assignee gate. That
/// gate can be lower than the configured value, and it is a different number
/// from `proofs.assigned`. A configured assignee count of 0, or a clamp to 0
/// because no eligible proof was assigned, waives that gate.
///
/// Counts are this node's stored proofs for the data root, deduplicated by
/// the stored signer. Heights are decimal strings. Counts are JSON numbers.
/// Null fields are present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressStatus {
    pub tx_id: H256,
    pub data_root: H256,
    #[serde(with = "u64_stringify")]
    pub as_of_block_height: u64,
    pub as_of_block_hash: H256,
    /// Block that promoted the transaction, or null when it has not.
    #[serde(with = "option_u64_stringify")]
    pub promotion_height: Option<u64>,
    /// Null once promoted. Present, with zero counts, for a confirmed empty cache.
    pub proofs: Option<IngressProofCounts>,
    /// Null once promoted.
    pub requirements: Option<IngressProofPair>,
    /// Null once promoted. Otherwise `max(0, requirement - eligible)` for each gate.
    pub remaining: Option<IngressProofPair>,
    /// Null once promoted. Otherwise true only when both deficits are zero.
    pub proof_requirements_met: Option<bool>,
}

/// Counts derived from [`IngressProofCounts::miners`].
///
/// `eligible` is the pool promotion would draw from before truncating it to
/// `requirements.total`. `miners` is one row per observed signer, in
/// first-seen cache order. `eligible: true` is an active proof. `eligible:
/// false` is stored but does not qualify. Miners who have not stored a proof
/// on this node are absent, so a deficit stays a count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressProofCounts {
    pub observed: u64,
    pub assigned: u64,
    pub unassigned: u64,
    pub eligible: u64,
    pub eligible_assigned: u64,
    pub eligible_unassigned: u64,
    pub miners: Vec<IngressProofSigner>,
}

/// `requirements` and `remaining` share this shape.
///
/// On `requirements`, `assigned` is the assignee gate. On `remaining`, both
/// fields are deficits against the eligible counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressProofPair {
    pub total: u64,
    pub assigned: u64,
}

struct CanonicalTip {
    height: u64,
    hash: H256,
    timestamp: UnixTimestamp,
    epoch: Arc<EpochSnapshot>,
}

/// GET /v1/tx/{tx_id}/ingress-status
///
/// Header lookup, then one canonical-tip read (height, hash, timestamp, epoch
/// snapshot), then promotion on that tip's branch. The proof cache is read only
/// when the transaction has not been promoted. `as_of_block_*` is that tip.
/// Counts treat it as the parent of the block that would be produced now.
pub async fn get_tx_ingress_status(
    state: web::Data<ApiState>,
    path: web::Path<H256>,
) -> Result<Json<IngressStatus>, ApiError> {
    let tx_id: H256 = path.into_inner();
    debug!("Get ingress status by tx_id: {}", tx_id);

    let header = lookup_data_tx_header(&state, tx_id).await?;
    let tip = read_canonical_tip(&state)?;
    // Publish membership on the tip's branch, the check the selector uses. The
    // header's `promoted_height` is a branch-variant local hint.
    let promotion_height = ingress_status::canonical_promotion_height(
        tx_id,
        tip.hash,
        tip.height,
        &state.config,
        &state.block_tree,
        &state.db,
    )
    .map_err(|e| ApiError::Internal {
        err: format!("promotion lookup failed: {e:#}"),
    })?;

    // A sufficient eligible count is not promotion. Only a recorded height is.
    let status =
        if let Some(height) = promotion_height {
            promoted_ingress_status(&header, &tip, height)
        } else {
            let query = IngressStatusQuery {
                parent_height: tip.height,
                requirement_timestamp: child_requirement_timestamp(tip.timestamp)?,
                block_tree: &state.block_tree,
                db: &state.db,
                config: &state.config,
                epoch_snapshot: &tip.epoch,
            };
            let assessment = ingress_status::load_submit_ingress_assessment(&header, &query)
                .map_err(|e| ApiError::Internal {
                    err: format!("ingress status assessment failed: {e:#}"),
                })?;
            unpromoted_ingress_status(&header, &tip, &assessment)
        };

    Ok(web::Json(status))
}

/// Mempool first, then the database. A read error is an internal failure.
/// Both stores missing the header is 404. A commitment id is not a data tx.
async fn lookup_data_tx_header(
    state: &web::Data<ApiState>,
    tx_id: H256,
) -> Result<DataTransactionHeader, ApiError> {
    match state.mempool_guard.get_data_txs(&[tx_id]).await {
        Ok(mut found) => {
            if let Some(header) = found.remove(&tx_id) {
                return Ok(header);
            }
        }
        Err(e) => {
            return Err(ApiError::Internal {
                err: format!("mempool data tx lookup failed: {e:#}"),
            });
        }
    }

    state
        .db
        .view_eyre(|tx| database::tx_header_by_txid(tx, &tx_id))
        .map_err(|e| ApiError::Internal {
            err: format!("data tx header lookup failed: {e:#}"),
        })?
        .ok_or(ApiError::ErrNoId {
            id: tx_id.to_string(),
            err: String::from("storage tx not found"),
        })
}

fn read_canonical_tip(state: &ApiState) -> Result<CanonicalTip, ApiError> {
    let block_tree = state.block_tree.read();
    let (chain, _) = block_tree.get_canonical_chain();
    let tip = chain.last().ok_or(ApiError::EmptyCanonicalChain)?;
    let hash = tip.block_hash();
    let epoch = block_tree
        .get_epoch_snapshot(&hash)
        .ok_or_else(|| ApiError::Internal {
            err: format!("missing epoch snapshot for canonical tip {hash}"),
        })?;
    Ok(CanonicalTip {
        height: tip.height(),
        hash,
        timestamp: tip.header().timestamp_secs(),
        epoch,
    })
}

/// Timestamp the next block would carry if it were produced now.
///
/// Hardfork proof counts switch on wall-clock time. This does not sleep: when
/// the clock is already past the parent second, that reading is used.
/// Otherwise the parent second plus one, including when the two seconds match.
fn child_requirement_timestamp(parent: UnixTimestamp) -> Result<UnixTimestamp, ApiError> {
    let now = UnixTimestamp::now().map_err(|e| ApiError::Internal {
        err: format!("system clock error: {e}"),
    })?;
    if now.as_secs() > parent.as_secs() {
        Ok(now)
    } else {
        Ok(UnixTimestamp::from_secs(parent.as_secs().saturating_add(1)))
    }
}

fn promoted_ingress_status(
    header: &DataTransactionHeader,
    tip: &CanonicalTip,
    promotion_height: u64,
) -> IngressStatus {
    IngressStatus {
        tx_id: header.id,
        data_root: header.data_root,
        as_of_block_height: tip.height,
        as_of_block_hash: tip.hash,
        promotion_height: Some(promotion_height),
        proofs: None,
        requirements: None,
        remaining: None,
        proof_requirements_met: None,
    }
}

fn unpromoted_ingress_status(
    header: &DataTransactionHeader,
    tip: &CanonicalTip,
    assessment: &IngressProofAssessment,
) -> IngressStatus {
    IngressStatus {
        tx_id: header.id,
        data_root: header.data_root,
        as_of_block_height: tip.height,
        as_of_block_hash: tip.hash,
        promotion_height: None,
        proofs: Some(IngressProofCounts {
            observed: assessment.observed(),
            assigned: assessment.assigned(),
            unassigned: assessment.unassigned(),
            eligible: assessment.eligible(),
            eligible_assigned: assessment.eligible_assigned(),
            eligible_unassigned: assessment.eligible_unassigned(),
            miners: assessment.signers.clone(),
        }),
        requirements: Some(IngressProofPair {
            total: assessment.required_total,
            assigned: assessment.required_assigned,
        }),
        remaining: Some(IngressProofPair {
            total: assessment.remaining_total(),
            assigned: assessment.remaining_assigned(),
        }),
        proof_requirements_met: Some(assessment.proof_requirements_met()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{
        DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata, DataTransactionMetadata,
        IrysAddress,
    };

    fn header(tx_id: H256, data_root: H256) -> DataTransactionHeader {
        DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: tx_id,
                data_root,
                ledger_id: u32::from(DataLedger::Submit),
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        })
    }

    fn tip(height: u64) -> CanonicalTip {
        CanonicalTip {
            height,
            hash: H256::repeat_byte(7),
            timestamp: UnixTimestamp::from_secs(1_700_000_000),
            epoch: Arc::new(EpochSnapshot::default()),
        }
    }

    fn signer(byte: u8, assigned: bool, eligible: bool) -> IngressProofSigner {
        IngressProofSigner {
            address: IrysAddress::repeat_byte(byte),
            assigned,
            eligible,
        }
    }

    fn partial_assessment() -> IngressProofAssessment {
        IngressProofAssessment {
            required_total: 6,
            required_assigned: 2,
            signers: vec![
                signer(1, true, true),
                signer(2, true, true),
                signer(3, false, true),
                signer(4, false, true),
                signer(5, false, false),
            ],
        }
    }

    #[test]
    fn ingress_status_json_contract_and_promotion_nulls() {
        let tx_id = H256::repeat_byte(1);
        let data_root = H256::repeat_byte(2);
        let header = header(tx_id, data_root);
        let as_of = tip(33_957);

        let awaiting = unpromoted_ingress_status(&header, &as_of, &partial_assessment());
        assert!(awaiting.promotion_height.is_none());
        assert_eq!(awaiting.proof_requirements_met, Some(false));

        let value = serde_json::to_value(&awaiting).expect("serialize");
        assert_eq!(value["txId"], serde_json::to_value(tx_id).unwrap());
        assert_eq!(value["dataRoot"], serde_json::to_value(data_root).unwrap());
        assert_eq!(value["asOfBlockHeight"], "33957");
        assert!(value["asOfBlockHash"].is_string());
        assert!(value.get("promotionHeight").is_some());
        assert!(value["promotionHeight"].is_null());
        assert!(value.get("tx_id").is_none());
        assert_eq!(value["proofs"]["observed"], 5);
        assert_eq!(value["proofs"]["assigned"], 2);
        assert_eq!(value["proofs"]["unassigned"], 3);
        assert_eq!(value["proofs"]["eligible"], 4);
        assert_eq!(value["proofs"]["eligibleAssigned"], 2);
        assert_eq!(value["proofs"]["eligibleUnassigned"], 2);
        assert!(value["proofs"]["observed"].is_u64());
        let miners = value["proofs"]["miners"].as_array().expect("miners");
        assert_eq!(miners.len(), 5);
        assert_eq!(
            miners
                .iter()
                .filter(|miner| miner["eligible"].as_bool() == Some(true))
                .count(),
            4
        );
        assert!(miners.iter().all(|miner| {
            miner["address"].is_string()
                && miner["assigned"].is_boolean()
                && miner["eligible"].is_boolean()
        }));
        assert_eq!(miners[0]["assigned"], true);
        assert_eq!(miners[4]["eligible"], false);
        assert_eq!(value["requirements"]["total"], 6);
        assert_eq!(value["requirements"]["assigned"], 2);
        assert_eq!(value["remaining"]["total"], 2);
        assert_eq!(value["remaining"]["assigned"], 0);
        assert_eq!(value["proofRequirementsMet"], false);

        let parsed: IngressStatus = serde_json::from_value(value).expect("round trip");
        assert_eq!(parsed, awaiting);

        // Eligible counts do not imply promotion. The assembler leaves the
        // height null; the handler sets it only from a promotion record.
        let met = IngressProofAssessment {
            required_total: 1,
            required_assigned: 0,
            signers: vec![signer(9, false, true)],
        };
        let ready = unpromoted_ingress_status(&header, &as_of, &met);
        assert!(ready.promotion_height.is_none());
        assert_eq!(ready.proof_requirements_met, Some(true));
        assert_eq!(ready.remaining.unwrap().total, 0);

        let promoted = promoted_ingress_status(&header, &tip(33_958), 33_958);
        assert_eq!(promoted.promotion_height, Some(33_958));
        assert!(promoted.proofs.is_none());
        assert!(promoted.requirements.is_none());
        assert!(promoted.remaining.is_none());
        assert!(promoted.proof_requirements_met.is_none());

        let promoted_json = serde_json::to_value(&promoted).expect("serialize promoted");
        assert_eq!(promoted_json["promotionHeight"], "33958");
        assert_eq!(promoted_json["asOfBlockHeight"], "33958");
        assert!(promoted_json.get("proofs").is_some());
        assert!(promoted_json["proofs"].is_null());
        assert!(
            promoted_json.get("miners").is_none(),
            "miner attribution is inside proofs and is null once promoted"
        );
        assert!(promoted_json.get("remaining").is_some());
        assert!(promoted_json["remaining"].is_null());
        assert!(promoted_json.get("proofRequirementsMet").is_some());
        assert!(promoted_json["proofRequirementsMet"].is_null());
        assert!(promoted_json.get("requirements").is_some());
        assert!(promoted_json["requirements"].is_null());
        let promoted_parsed: IngressStatus =
            serde_json::from_value(promoted_json).expect("promoted round trip");
        assert_eq!(promoted_parsed, promoted);
    }

    #[test]
    fn ingress_status_child_timestamp_is_strictly_after_the_parent() {
        let now = UnixTimestamp::now().expect("system clock");
        let future = UnixTimestamp::from_secs(now.as_secs().saturating_add(10_000));
        let after_future = child_requirement_timestamp(future).expect("system clock");
        assert_eq!(after_future.as_secs(), future.as_secs().saturating_add(1));

        let past = UnixTimestamp::from_secs(now.as_secs().saturating_sub(10_000));
        let after_past = child_requirement_timestamp(past).expect("system clock");
        assert!(after_past.as_secs() > past.as_secs());
    }
}
