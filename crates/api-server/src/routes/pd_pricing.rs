//! PD (Programmable Data) pricing API endpoints.
//!
//! Provides a unified endpoint for querying PD base fees, utilization, and priority fees
//! following Ethereum's eth_feeHistory pattern.

use actix_web::{web, HttpResponse};

use crate::{error::ApiError, ApiState};

// Re-export response types from actors module
pub use irys_actors::pd_pricing::{
    BlockBaseFee, BlockPriorityFees, BlockUtilization, FeeHistoryAnalysis,
    PdFeeHistoryResponse, PriorityFeeAtPercentile,
};

// Query Parameters

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistoryQuery {
    block_count: u64,
    #[serde(default = "default_percentiles")]
    reward_percentiles: Vec<u8>,
}

fn default_percentiles() -> Vec<u8> {
    vec![25, 50, 75]
}

// API Endpoint

/// GET /v1/price/pd/fee-history?blockCount=N&rewardPercentiles=25,50,75
///
/// Returns comprehensive fee history (Ethereum eth_feeHistory style).
///
/// Provides per-block data for:
/// - Base fees (in Irys and USD)
/// - PD utilization metrics
/// - Priority fee percentiles
/// - Next block base fee prediction (N+1)
///
/// Query parameters:
/// - `blockCount` (required): Number of recent blocks to analyze (1-1000)
/// - `rewardPercentiles` (optional): Priority fee percentiles to calculate (0-100, max 10)
///   Defaults to [25, 50, 75]
///
/// Example: GET /v1/price/pd/fee-history?blockCount=20&rewardPercentiles=10,50,90
pub async fn get_fee_history(
    query: web::Query<FeeHistoryQuery>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    // Validate block_count
    if query.block_count == 0 || query.block_count > 1000 {
        return Err((
            "block_count must be between 1 and 1000".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }

    // Validate percentiles
    if query.reward_percentiles.is_empty() {
        return Err((
            "At least one percentile must be specified".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }
    if query.reward_percentiles.len() > 10 {
        return Err((
            "Maximum 10 percentiles allowed".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }
    for &p in &query.reward_percentiles {
        if p > 100 {
            return Err((
                format!("Percentiles must be 0-100, got {}", p),
                actix_web::http::StatusCode::BAD_REQUEST,
            )
                .into());
        }
    }

    let response = state
        .pd_pricing
        .get_fee_history(query.block_count, &query.reward_percentiles)
        .map_err(|e| {
            (
                format!("Failed to get fee history: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_history_default_percentiles() {
        let query = FeeHistoryQuery {
            block_count: 20,
            reward_percentiles: default_percentiles(),
        };
        assert_eq!(query.reward_percentiles, vec![25, 50, 75]);
    }

    #[test]
    fn test_fee_history_custom_percentiles() {
        let query = FeeHistoryQuery {
            block_count: 50,
            reward_percentiles: vec![10, 50, 90],
        };
        assert_eq!(query.reward_percentiles, vec![10, 50, 90]);
    }
}
