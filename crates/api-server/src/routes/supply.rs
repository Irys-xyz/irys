use actix_web::{web, HttpResponse, Responder};
use eyre::{eyre, Result};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_reward_curve::{GenesisRelativeTimestamp, HalvingCurve};
use irys_types::U256;
use serde::{Deserialize, Serialize};

use crate::ApiState;

const TOKEN_DECIMALS: u32 = 18;
const BILLION_DECIMALS: u32 = 9;
const BILLION_SCALE_POW: u32 = TOKEN_DECIMALS + BILLION_DECIMALS;
const BILLION_DECIMAL_PLACES: u32 = 6;
const PERCENT_SCALE: u128 = 10000;
const PERCENT_DIVISOR: u128 = 100;

#[derive(Debug, Deserialize)]
pub struct SupplyQuery {
    #[serde(default)]
    pub estimate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupplyResponse {
    pub total_supply: String,
    pub total_supply_billions: String,
    pub genesis_supply: String,
    pub genesis_supply_billions: String,
    pub emitted_supply: String,
    pub emitted_supply_billions: String,
    pub timestamp_millis: u128,
    pub block_height: u64,
    pub inflation_cap: String,
    pub inflation_cap_billions: String,
    pub inflation_progress_percent: String,
    pub calculation_method: String,
}

/// GET /supply
///
/// Returns the current total token supply including genesis allocation and emissions
///
/// Query parameters:
/// - `estimate`: boolean (optional) - If true, uses fast formula-based calculation. Default: false (sums actual block rewards)
///
/// Default behavior sums the actual `reward_amount` from all blocks in the canonical chain,
/// ensuring 100% accuracy. Use `?estimate=true` for calculation.
pub async fn supply(state: web::Data<ApiState>, query: web::Query<SupplyQuery>) -> impl Responder {
    match calculate_supply(&state, query.estimate) {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(e) => {
            HttpResponse::InternalServerError().body(format!("Error calculating supply: {}", e))
        }
    }
}

fn calculate_supply(state: &ApiState, use_estimate: bool) -> Result<SupplyResponse> {
    let tree = state.block_tree.read();
    let (canonical, _) = tree.get_canonical_chain();

    let last_entry = canonical
        .last()
        .ok_or_else(|| eyre!("No blocks in canonical chain"))?;

    let last_block = state
        .db
        .view_eyre(|tx| block_header_by_hash(tx, &last_entry.block_hash, false))?
        .ok_or_else(|| eyre!("Block header not found for tip"))?;

    let current_timestamp_millis = last_block.timestamp;
    let block_height = last_block.height;

    let config = &state.config.consensus;
    let genesis_timestamp_millis = config.genesis.timestamp_millis;

    let genesis_supply: U256 = config
        .reth
        .genesis
        .alloc
        .values()
        .fold(U256::zero(), |acc, account| {
            acc + U256::from_le_bytes(account.balance.to_le_bytes())
        });

    let (emitted_amount, calculation_method) = if use_estimate {
        let elapsed =
            GenesisRelativeTimestamp::new(genesis_timestamp_millis, current_timestamp_millis)?;

        let curve = HalvingCurve {
            inflation_cap: config.block_reward_config.inflation_cap,
            half_life_secs: config.block_reward_config.half_life_secs as u128,
        };

        let emitted = curve.total_emitted_estimated(elapsed)?;
        (emitted.amount, "estimated")
    } else {
        let total_emitted = state.db.view_eyre(|tx| {
            let mut sum = U256::zero();
            for entry in canonical.iter() {
                if let Some(block) = block_header_by_hash(tx, &entry.block_hash, false)? {
                    sum += block.reward_amount;
                }
            }
            Ok(sum)
        })?;

        (total_emitted, "actual")
    };

    let total_supply = genesis_supply + emitted_amount;

    Ok(SupplyResponse {
        total_supply: total_supply.to_string(),
        total_supply_billions: format_to_billions(total_supply),
        genesis_supply: genesis_supply.to_string(),
        genesis_supply_billions: format_to_billions(genesis_supply),
        emitted_supply: emitted_amount.to_string(),
        emitted_supply_billions: format_to_billions(emitted_amount),
        timestamp_millis: current_timestamp_millis,
        block_height,
        inflation_cap: config.block_reward_config.inflation_cap.amount.to_string(),
        inflation_cap_billions: format_to_billions(config.block_reward_config.inflation_cap.amount),
        inflation_progress_percent: calculate_inflation_progress(
            emitted_amount,
            config.block_reward_config.inflation_cap.amount,
        ),
        calculation_method: calculation_method.to_string(),
    })
}

fn format_to_billions(amount: U256) -> String {
    let billion_scale = U256::from(10_u128.pow(BILLION_SCALE_POW));

    if amount.is_zero() {
        return "0.000000".to_string();
    }

    let billions = amount / billion_scale;
    let remainder = amount % billion_scale;

    let decimals = (remainder * U256::from(10_u128.pow(BILLION_DECIMAL_PLACES))) / billion_scale;
    let decimals_u128: u128 = decimals.try_into().unwrap_or(0);

    format!("{}.{:06}", billions, decimals_u128)
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
    #[case(0_u128, "0.000000")]
    #[case(10_000_000_000_u128, "10.000000")]
    #[case(1_500_000_000_u128, "1.500000")]
    #[case(123_456_u128, "0.000123")]
    fn test_format_to_billions(#[case] amount: u128, #[case] expected: &str) {
        let amount_with_decimals = U256::from(amount) * U256::from(10_u128.pow(18));
        assert_eq!(format_to_billions(amount_with_decimals), expected);
    }

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
