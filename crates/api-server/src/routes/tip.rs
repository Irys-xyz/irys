use crate::{ApiState, error::ApiError};
use actix_web::{HttpResponse, ResponseError as _, http::header::ContentType, web};
use awc::http::StatusCode;
use irys_domain::get_canonical_chain;
use irys_types::{H256, U256};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TipResponse {
    pub hash: H256,
    pub height: u64,
    pub cumulative_diff: U256,
    pub parent_hash: H256,
    pub last_reorg_at: i64,
    pub last_block_at: i64,
}

pub async fn tip_route(state: web::Data<ApiState>) -> HttpResponse {
    let (chain, _pending) = match get_canonical_chain(state.block_tree.clone()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read canonical chain");
            return ApiError::from((
                "Failed to read canonical chain",
                StatusCode::INTERNAL_SERVER_ERROR,
            ))
            .error_response();
        }
    };
    let Some(tip) = chain.last() else {
        return ApiError::from(("No canonical tip", StatusCode::SERVICE_UNAVAILABLE))
            .error_response();
    };
    let header = tip.header();
    let body = TipResponse {
        hash: tip.block_hash(),
        height: tip.height(),
        cumulative_diff: header.cumulative_diff,
        parent_hash: header.previous_block_hash,
        last_reorg_at: state.block_tree_lifecycle.last_reorg_at_ms(),
        last_block_at: state.block_tree_lifecycle.last_block_at_ms(),
    };
    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_response_serialises_with_documented_field_set() {
        let body = TipResponse {
            hash: H256([1; 32]),
            height: 42,
            cumulative_diff: U256::from(1_000_u64),
            parent_hash: H256([2; 32]),
            last_reorg_at: 1_700_000_000_000,
            last_block_at: 1_700_000_001_000,
        };
        let value: serde_json::Value = serde_json::to_value(&body).expect("serialise");
        let object = value.as_object().expect("top-level JSON object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "cumulative_diff",
                "hash",
                "height",
                "last_block_at",
                "last_reorg_at",
                "parent_hash",
            ],
            "documented /v1/tip JSON field set drifted"
        );
        assert_eq!(object["height"], 42);
        assert_eq!(object["last_block_at"], 1_700_000_001_000_i64);
        assert_eq!(object["last_reorg_at"], 1_700_000_000_000_i64);
    }

    #[test]
    fn tip_response_round_trips_through_json() {
        let body = TipResponse {
            hash: H256([3; 32]),
            height: 7,
            cumulative_diff: U256::from(99_u64),
            parent_hash: H256([4; 32]),
            last_reorg_at: 0,
            last_block_at: 0,
        };
        let json = serde_json::to_string(&body).expect("serialise");
        let parsed: TipResponse = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(parsed, body);
    }
}
