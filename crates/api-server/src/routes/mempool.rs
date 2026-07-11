use crate::ApiState;
use actix_web::{HttpResponse, Result, web};
use irys_actors::mempool_service::{
    MEMPOOL_TXS_DEFAULT_LIMIT, MEMPOOL_TXS_MAX_LIMIT, MempoolPendingTxs,
};
use serde::Deserialize;

/// GET /v1/mempool/status
///
/// Aggregate mempool counters (counts, capacity %). Unchanged for existing
/// count-only consumers — use `/v1/mempool/txs` for the pending list.
pub async fn get_mempool_status(state: web::Data<ApiState>) -> Result<HttpResponse> {
    let status = state
        .mempool_guard
        .atomic_state()
        .get_status(&state.config.node_config, &state.chunk_ingress_state)
        .await;
    Ok(HttpResponse::Ok().json(status))
}

/// Query params for `GET /v1/mempool/txs`.
#[derive(Debug, Deserialize)]
pub struct MempoolTxsQuery {
    /// Max entries per list (`data_txs` / `commitment_txs`). Default 100, cap 500.
    pub limit: Option<usize>,
    /// Forward cursor (base58 `H256`): return ids strictly after this value.
    pub after_id: Option<String>,
}

/// GET /v1/mempool/txs — light unconfirmed pending-tx list (empty pool → 200 + `[]`).
///
/// Use `?after_id=` when `truncated` is true. See `MempoolPendingTxs`.
pub async fn get_mempool_txs(
    state: web::Data<ApiState>,
    query: web::Query<MempoolTxsQuery>,
) -> Result<HttpResponse> {
    let limit = query
        .limit
        .unwrap_or(MEMPOOL_TXS_DEFAULT_LIMIT)
        .clamp(1, MEMPOOL_TXS_MAX_LIMIT);

    let after_id = match query.after_id.as_deref() {
        Some(s) => match irys_types::H256::from_base58_result(s) {
            Ok(id) => Some(id),
            Err(e) => {
                return Ok(HttpResponse::BadRequest()
                    .body(format!("invalid after_id (expected base58 H256): {e}")));
            }
        },
        None => None,
    };

    let pending: MempoolPendingTxs = state
        .mempool_guard
        .atomic_state()
        .get_pending_txs(
            state.config.consensus.chunk_size,
            &state.chunk_ingress_state,
            limit,
            after_id,
        )
        .await;
    Ok(HttpResponse::Ok().json(pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_actors::mempool_service::{MempoolPendingCommitmentTx, MempoolPendingDataTx};
    use irys_types::{H256, IrysAddress};

    #[test]
    fn mempool_pending_txs_empty_serialises_with_zero_counts() {
        let body = MempoolPendingTxs::empty(0);
        let value: serde_json::Value = serde_json::to_value(&body).expect("serialise");
        let object = value.as_object().expect("top-level JSON object");

        assert_eq!(object["data_txs"], serde_json::json!([]));
        assert_eq!(object["commitment_txs"], serde_json::json!([]));
        assert_eq!(object["data_tx_count"], 0);
        assert_eq!(object["commitment_tx_count"], 0);
        assert_eq!(object["pending_chunks_count"], 0);
        assert_eq!(object["truncated"], false);
        assert!(object.get("total_data_tx_count").is_none());
        assert!(object.get("total_commitment_tx_count").is_none());
    }

    #[test]
    fn mempool_pending_txs_truncation_fields_round_trip() {
        let id = H256([1; 32]);
        let body = MempoolPendingTxs {
            data_txs: vec![MempoolPendingDataTx {
                id,
                byte_size: 512,
                chunks: 1,
                data_root: H256([2; 32]),
                ledger_id: 1,
            }],
            commitment_txs: vec![MempoolPendingCommitmentTx {
                id: H256([3; 32]),
                address: IrysAddress::ZERO,
            }],
            data_tx_count: 1,
            commitment_tx_count: 1,
            pending_chunks_count: 4,
            truncated: true,
            total_data_tx_count: Some(250),
            total_commitment_tx_count: Some(10),
        };
        let json = serde_json::to_string(&body).expect("serialise");
        let parsed: MempoolPendingTxs = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(parsed, body);
        // ids must be base58 strings in JSON (same as /v1/tx/{id})
        let value: serde_json::Value = serde_json::from_str(&json).expect("value");
        assert!(value["data_txs"][0]["id"].as_str().is_some());
        assert_eq!(value["data_txs"][0]["byte_size"], 512);
        assert_eq!(value["data_txs"][0]["chunks"], 1);
        assert_eq!(value["total_data_tx_count"], 250);
    }

    #[test]
    fn limit_clamp_matches_documented_bounds() {
        assert_eq!(0_usize.clamp(1, MEMPOOL_TXS_MAX_LIMIT), 1);
        assert_eq!(
            MEMPOOL_TXS_DEFAULT_LIMIT.clamp(1, MEMPOOL_TXS_MAX_LIMIT),
            MEMPOOL_TXS_DEFAULT_LIMIT
        );
        assert_eq!(
            10_000_usize.clamp(1, MEMPOOL_TXS_MAX_LIMIT),
            MEMPOOL_TXS_MAX_LIMIT
        );
    }
}
