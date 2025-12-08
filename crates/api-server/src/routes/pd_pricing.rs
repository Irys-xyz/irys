//! PD (Programmable Data) pricing API endpoints.
//!
//! Provides public-facing endpoints for querying PD base fees, estimating future fees,
//! and analyzing priority fees for PD transactions.

use actix_web::{web, HttpResponse};

use crate::{error::ApiError, ApiState};

// Re-export response types from actors module
pub use irys_actors::pd_pricing::{
    PdBaseFeeEstimate, PdBaseFeeHistory, PdBaseFeeHistoryItem, PdBaseFeeResponse, PdFeeAtHeight,
    PdPriorityFeeEstimate,
};

//==============================================================================
// API Endpoints
//==============================================================================

/// GET /v1/price/pd/base-fee
///
/// Returns the current PD base fee per chunk.
pub async fn get_current_base_fee_pd(
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let response = state
        .pd_pricing
        .get_current_base_fee()
        .map_err(|e| {
            (
                format!("Failed to get current base fee: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}

/// GET /v1/price/pd/estimate/{blocks_ahead}
///
/// Projects future PD base fee using moving average utilization.
pub async fn estimate_base_fee_pd(
    path: web::Path<u64>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let blocks_ahead = path.into_inner();

    // Validate input
    if blocks_ahead == 0 || blocks_ahead > 1000 {
        return Err((
            "blocks_ahead must be between 1 and 1000".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }

    let response = state
        .pd_pricing
        .estimate_base_fee(blocks_ahead)
        .map_err(|e| {
            (
                format!("Failed to estimate base fee: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}

/// GET /v1/price/pd/priority-fee/{target_blocks}
///
/// Suggests priority fee per chunk for inclusion within N blocks.
///
/// This endpoint analyzes recent PD transactions to estimate appropriate priority fees.
/// It returns three recommendations (low/medium/high) based on historical data and current utilization.
pub async fn estimate_priority_fee_pd(
    path: web::Path<u64>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let target_blocks = path.into_inner();

    // Validate input
    if target_blocks == 0 || target_blocks > 100 {
        return Err((
            "target_blocks must be between 1 and 100".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }

    let response = state
        .pd_pricing
        .estimate_priority_fee(target_blocks)
        .map_err(|e| {
            (
                format!("Failed to estimate priority fee: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}

/// GET /v1/price/pd/history/{blocks}
///
/// Returns historical PD base fees.
pub async fn get_base_fee_history(
    path: web::Path<u64>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let requested_blocks = path.into_inner();

    // Validate input
    if requested_blocks == 0 || requested_blocks > 1000 {
        return Err((
            "blocks must be between 1 and 1000".to_string(),
            actix_web::http::StatusCode::BAD_REQUEST,
        )
            .into());
    }

    let response = state
        .pd_pricing
        .get_base_fee_history(requested_blocks)
        .map_err(|e| {
            (
                format!("Failed to get base fee history: {}", e),
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;

    Ok(HttpResponse::Ok().json(response))
}
