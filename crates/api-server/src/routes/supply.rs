use actix_web::{web, HttpResponse, Responder, ResponseError as _};
use awc::http::StatusCode;
use eyre::{eyre, Result};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_reward_curve::HalvingCurve;
use irys_types::IrysBlockHeader;
use irys_types::{serialization::string_u64, U256};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::ApiState;

const PERCENT_SCALE: u128 = 10000;
const PERCENT_DIVISOR: u128 = 100;

#[derive(Debug, Deserialize)]
pub struct SupplyQuery {
    #[serde(default)]
    pub exact: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyResponse {
    pub total_supply: String,
    pub genesis_supply: String,
    pub emitted_supply: String,
    #[serde(with = "string_u64")]
    pub block_height: u64,
    pub inflation_cap: String,
    pub inflation_progress_percent: String,
    pub calculation_method: String,
}

/// Returns current total token supply including genesis allocation and emissions.
pub async fn supply(state: web::Data<ApiState>, query: web::Query<SupplyQuery>) -> impl Responder {
    match calculate_supply(&state, query.exact) {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(e) => ApiError::CustomWithStatus(
            format!("Error calculating supply: {}", e),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .error_response(),
    }
}

fn get_latest_block(state: &ApiState) -> Result<IrysBlockHeader> {
    let tree = state.block_tree.read();
    let (canonical, _) = tree.get_canonical_chain();

    let last_entry = canonical
        .last()
        .ok_or_else(|| eyre!("No blocks in canonical chain"))?;

    if let Some(block) = tree.get_block(&last_entry.block_hash) {
        return Ok(block.clone());
    }

    state
        .db
        .view_eyre(|tx| block_header_by_hash(tx, &last_entry.block_hash, false))?
        .ok_or_else(|| eyre!("Block header not found for tip in tree or database"))
}

fn calculate_supply(state: &ApiState, use_exact: bool) -> Result<SupplyResponse> {
    let block_height = get_latest_block(state)?.height;

    let config = &state.config.consensus;
    let genesis_supply = config.genesis_supply();

    let (emitted_amount, calculation_method) = if use_exact {
        let supply_state = state.supply_state.as_ref().ok_or_else(|| {
            eyre!(
                "Supply state not available for exact calculation. \
                Use exact=false for estimated supply."
            )
        })?;

        if supply_state.is_failed() {
            return Err(eyre!(
                "Supply state initialization failed. \
                Use exact=false for estimated supply."
            ));
        }

        if !supply_state.is_ready() {
            return Err(eyre!(
                "Supply state still initializing. \
                Use exact=false for estimated supply, or retry later."
            ));
        }

        (supply_state.get().cumulative_emitted, "actual")
    } else {
        calculate_estimated_emission(config, block_height)?
    };

    let total_supply = genesis_supply + emitted_amount;

    Ok(SupplyResponse {
        total_supply: total_supply.to_string(),
        genesis_supply: genesis_supply.to_string(),
        emitted_supply: emitted_amount.to_string(),
        block_height,
        inflation_cap: config.block_reward_config.inflation_cap.amount.to_string(),
        inflation_progress_percent: calculate_inflation_progress(
            emitted_amount,
            config.block_reward_config.inflation_cap.amount,
        ),
        calculation_method: calculation_method.to_string(),
    })
}

fn calculate_estimated_emission(
    config: &irys_types::ConsensusConfig,
    block_height: u64,
) -> Result<(U256, &'static str)> {
    let curve = HalvingCurve {
        inflation_cap: config.block_reward_config.inflation_cap,
        half_life_secs: config.block_reward_config.half_life_secs as u128,
    };

    let target_block_time_seconds = config.difficulty_adjustment.block_time as u128;
    let simulated_time_seconds = block_height as u128 * target_block_time_seconds;

    let emitted = curve.reward_between(0, simulated_time_seconds)?;
    Ok((emitted.amount, "estimated"))
}

fn calculate_inflation_progress(emitted: U256, cap: U256) -> String {
    if cap.is_zero() {
        return "0.00".to_string();
    }

    let progress = (emitted * U256::from(PERCENT_SCALE)) / cap;
    let whole = progress / U256::from(PERCENT_DIVISOR);
    let frac = progress % U256::from(PERCENT_DIVISOR);

    let whole_u128: u128 = whole.try_into().unwrap_or(0);
    let frac_u128: u128 = frac.try_into().unwrap_or(0);

    format!("{}.{:02}", whole_u128, frac_u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(0_u128, "0.00")]
    #[case(650_000_000_u128, "50.00")]
    #[case(1_300_000_000_u128, "100.00")]
    #[case(325_000_000_u128, "25.00")]
    fn test_calculate_inflation_progress(#[case] emitted: u128, #[case] expected: &str) {
        let cap = U256::from(1_300_000_000_u128) * U256::from(10_u128.pow(18));
        let emitted_amount = U256::from(emitted) * U256::from(10_u128.pow(18));
        assert_eq!(calculate_inflation_progress(emitted_amount, cap), expected);
    }

    #[test]
    fn test_calculate_inflation_progress_with_decimals() {
        let cap = U256::from(1_300_000_000_u128) * U256::from(10_u128.pow(18));
        let amount = (cap * U256::from(1234_u128)) / U256::from(10000_u128);
        assert_eq!(calculate_inflation_progress(amount, cap), "12.34");
    }
}
