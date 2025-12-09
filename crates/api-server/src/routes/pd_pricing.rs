//! PD (Programmable Data) pricing API endpoints.
//!
//! Provides a unified endpoint for querying PD base fees, utilization, and priority fees
//! following Ethereum's eth_feeHistory pattern.

use actix_web::{web, HttpResponse};

use crate::{error::ApiError, ApiState};

// Re-export response types from actors module
pub use irys_actors::pd_pricing::{
    BlockPriorityFees, PdFeeHistoryResponse, PriorityFeeAtPercentile,
};

/// Hardcoded percentiles for priority fee calculations
const REWARD_PERCENTILES: &[u8] = &[25, 50, 75];

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistoryQuery {
    block_count: u64,
}

/// GET /v1/price/pd/fee-history?blockCount=N
///
/// Returns comprehensive fee history (Ethereum eth_feeHistory style).
///
/// Provides per-block data for:
/// - Base fees (in Irys and USD)
/// - PD utilization metrics
/// - Priority fee percentiles (25th, 50th, 75th)
/// - Next block base fee prediction (N+1)
///
/// Query parameters:
/// - `blockCount` (required): Number of recent blocks to analyze (1-1000)
///
/// Example: GET /v1/price/pd/fee-history?blockCount=20
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

    let response = state
        .pd_pricing
        .get_fee_history(query.block_count, REWARD_PERCENTILES)
        .map_err(|e| {
            (
                format!("Failed to get fee history: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}
