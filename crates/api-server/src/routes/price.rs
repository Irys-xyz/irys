use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use base58::FromBase58 as _;
use irys_types::{
    serialization::string_u64,
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee},
    transaction::{CommitmentTransaction, PledgeDataProvider as _},
    Address, DataLedger, U256,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;

use crate::ApiState;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PriceInfo {
    pub perm_fee: U256,
    pub term_fee: U256,
    pub ledger: u32,
    #[serde(with = "string_u64")]
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentPriceInfo {
    pub value: U256,
    pub fee: U256,
    pub user_address: Option<Address>,
    pub pledge_count: Option<u64>,
}

pub async fn get_price(
    path: Path<(u32, u64)>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let (ledger, bytes_to_store) = path.into_inner();
    let chunk_size = state.config.consensus.chunk_size;

    // Convert ledger to enum, or bail out with an HTTP 400
    let data_ledger =
        DataLedger::try_from(ledger).map_err(|_| ErrorBadRequest("Ledger type not supported"))?;

    // enforce that the requested size is at least equal to a single chunk
    let bytes_to_store = std::cmp::max(chunk_size, bytes_to_store);

    // round up to the next multiple of chunk_size
    let bytes_to_store = bytes_to_store.div_ceil(chunk_size) * chunk_size;

    match data_ledger {
        DataLedger::Publish => {
            // Get the latest EMA for pricing calculations and the tip block
            let tree = state.block_tree.read();
            let tip = tree.tip;
            let ema = tree
                .get_ema_snapshot(&tip)
                .ok_or_else(|| ErrorBadRequest("EMA snapshot not available"))?;

            // Calculate the actual epochs remaining for the next block based on height
            let tip_height = tree.get_block(&tip).map(|b| b.height).unwrap_or(0);
            let next_block_height = tip_height + 1;

            let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
                next_block_height,
                state.config.consensus.epoch.num_blocks_in_epoch,
                state.config.consensus.epoch.submit_ledger_epoch_length,
            );

            drop(tree);

            // Determine pricing EMA based on proximity to interval boundary
            // When near the end of an interval, use max pricing to ensure transaction
            // fees remain sufficient even if the transaction is included after the interval rolls over
            let price_adjustment_interval = state.config.consensus.ema.price_adjustment_interval;
            let position_in_interval = next_block_height % price_adjustment_interval;
            let blocks_until_boundary = if position_in_interval == 0 {
                0  // Exactly at interval boundary
            } else {
                price_adjustment_interval - position_in_interval
            };

            // Last 25% of interval: use max(current_pricing, next_interval_pricing)
            let last_quarter_size = (price_adjustment_interval + 3) / 4;  // Ceiling division
            let in_last_quarter = blocks_until_boundary > 0 && blocks_until_boundary <= last_quarter_size;

            let pricing_ema = if in_last_quarter {
                // Protect against price increases when interval boundary is crossed
                // Use the higher of current or next interval's pricing
                if ema.ema_price_1_interval_ago.amount > ema.ema_price_2_intervals_ago.amount {
                    ema.ema_price_1_interval_ago
                } else {
                    ema.ema_price_2_intervals_ago
                }
            } else {
                // Normal case: use standard public pricing (2 intervals ago)
                ema.ema_for_public_pricing()
            };

            // Calculate term fee using the dynamic epoch count
            let term_fee = calculate_term_fee(
                bytes_to_store,
                epochs_for_storage,
                &state.config.consensus,
                pricing_ema,
            )
            .map_err(|e| ErrorBadRequest(format!("Failed to calculate term fee: {e:?}")))?;

            // Debug logging for fee calculation
            tracing::debug!(
                "Fee calculation - bytes: {}, term_fee: {}, inclusion_reward_percent raw: {}, num_ingress_proofs: {}",
                bytes_to_store,
                term_fee,
                state.config.consensus.immediate_tx_inclusion_reward_percent.amount,
                state.config.consensus.number_of_ingress_proofs_total
            );

            // If the cost calculation fails, return 400 with the error text
            let total_perm_cost = calculate_perm_fee_from_config(
                bytes_to_store,
                &state.config.consensus,
                pricing_ema,
                term_fee,
            )
            .map_err(|e| ErrorBadRequest(format!("{e:?}")))?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                perm_fee: total_perm_cost.amount,
                term_fee,
                ledger,
                bytes: bytes_to_store,
            }))
        }
        // TODO: support other term ledgers here
        DataLedger::Submit => Err(ErrorBadRequest("Term ledger not supported")),
    }
}

pub async fn get_stake_price(state: web::Data<ApiState>) -> ActixResult<HttpResponse> {
    let stake_value = state.config.consensus.stake_value;
    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: stake_value.amount,
        fee: U256::from(commitment_fee),
        user_address: None,
        pledge_count: None,
    }))
}

pub async fn get_unstake_price(state: web::Data<ApiState>) -> ActixResult<HttpResponse> {
    let stake_value = state.config.consensus.stake_value;
    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: stake_value.amount,
        fee: U256::from(commitment_fee),
        user_address: None,
        pledge_count: None,
    }))
}

/// Parse and validate a user address from a string
fn parse_user_address(address_str: &str) -> Result<Address, actix_web::Error> {
    // try Base58 format first
    if let Ok(decoded) = address_str.from_base58() {
        if let Ok(arr) = TryInto::<[u8; 20]>::try_into(decoded.as_slice()) {
            return Ok(Address::from(&arr));
        }
    }

    // fall back to hex/EVM address format
    Address::from_str(address_str).map_err(|_| ErrorBadRequest("Invalid address format"))
}

pub async fn get_pledge_price(
    path: Path<String>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let user_address_str = path.into_inner();
    let user_address = parse_user_address(&user_address_str)?;

    // Use the MempoolPledgeProvider to get accurate pledge count
    let pledge_count = state
        .mempool_pledge_provider
        .pledge_count(user_address)
        .await;

    // Calculate the pledge value using the same logic as CommitmentTransaction
    let pledge_value = CommitmentTransaction::calculate_pledge_value_at_count(
        &state.config.consensus,
        pledge_count,
    );

    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: pledge_value,
        fee: U256::from(commitment_fee),
        user_address: Some(user_address),
        pledge_count: Some(pledge_count),
    }))
}

pub async fn get_unpledge_price(
    path: Path<String>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let user_address_str = path.into_inner();
    let user_address = parse_user_address(&user_address_str)?;

    // Use the MempoolPledgeProvider to get accurate pledge count
    let pledge_count = state
        .mempool_pledge_provider
        .pledge_count(user_address)
        .await;

    // Calculate refund amount using the same logic as CommitmentTransaction
    let refund_amount = if pledge_count == 0 {
        U256::zero()
    } else {
        CommitmentTransaction::calculate_pledge_value_at_count(
            &state.config.consensus,
            pledge_count.saturating_sub(1),
        )
    };

    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: refund_amount,
        fee: U256::from(commitment_fee),
        user_address: Some(user_address),
        pledge_count: Some(pledge_count),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_price_info_bytes_serialization() {
        let price_info = PriceInfo {
            perm_fee: U256::from(1000),
            term_fee: U256::from(2000),
            ledger: 1,
            bytes: u64::MAX,
        };

        let json = serde_json::to_string(&price_info).unwrap();
        assert!(json.contains(&format!("\"bytes\":\"{}\"", u64::MAX)));

        let deserialized: PriceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.bytes, u64::MAX);
    }

    #[test]
    fn test_price_info_javascript_compatibility() {
        const JS_MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1; // 2^53 - 1
        let above_safe_limit = JS_MAX_SAFE_INTEGER + 1;

        let price_info = PriceInfo {
            perm_fee: U256::from(1000),
            term_fee: U256::from(2000),
            ledger: 1,
            bytes: above_safe_limit,
        };

        let json = serde_json::to_string(&price_info).unwrap();

        // Should be string, not number
        assert!(json.contains(&format!("\"bytes\":\"{above_safe_limit}\"")));

        // Test JavaScript parsing would work
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        if let Some(bytes_str) = parsed.get("bytes").and_then(|v| v.as_str()) {
            let parsed_bytes: u64 = bytes_str.parse().unwrap();
            assert_eq!(parsed_bytes, above_safe_limit);
        } else {
            panic!("bytes should be serialized as string");
        }
    }
}
