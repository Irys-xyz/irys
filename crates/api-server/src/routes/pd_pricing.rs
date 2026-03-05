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
/// - `blockCount` (required): Number of recent blocks to analyze (1 to block_tree_depth)
///
/// Example: GET /v1/price/pd/fee-history?blockCount=20
///
/// Returns 503 Service Unavailable if the Sprite hardfork is not yet active.
pub async fn get_fee_history(
    query: web::Query<FeeHistoryQuery>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    // Check if Sprite hardfork is active
    let current_timestamp = irys_types::UnixTimestamp::now().map_err(|e| {
        (
            format!("Failed to get current time: {}", e),
            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;
    if !state
        .config
        .consensus
        .hardforks
        .is_sprite_active(current_timestamp)
    {
        return Err((
            "PD pricing unavailable: Sprite hardfork not yet active",
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
        )
            .into());
    }

    let max_block_count = state.config.consensus.block_tree_depth;

    // Validate block_count
    if query.block_count == 0 || query.block_count > max_block_count {
        return Err((
            format!("block_count must be between 1 and {}", max_block_count),
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
