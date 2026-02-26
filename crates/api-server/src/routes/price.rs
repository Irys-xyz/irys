use actix_web::{
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use awc::http::StatusCode;
use irys_types::{
    serialization::string_u64,
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee},
    CommitmentTransaction, DataLedger, IrysAddress, PledgeDataProvider as _, U256,
};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, ApiState};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_address: Option<IrysAddress>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        with = "irys_types::serialization::optional_string_u64"
    )]
    pub pledge_count: Option<u64>,
}

pub async fn get_price(
    path: Path<(u32, u64)>,
    state: web::Data<ApiState>,
) -> Result<HttpResponse, ApiError> {
    let (ledger, bytes_to_store) = path.into_inner();
    let chunk_size = state.config.consensus.chunk_size;

    // Convert ledger to enum, or bail out with an HTTP 400
    let data_ledger = DataLedger::try_from(ledger)
        .map_err(|_| ("Ledger type not supported", StatusCode::BAD_REQUEST))?;

    // enforce that the requested size is at least equal to a single chunk
    let bytes_to_store = std::cmp::max(chunk_size, bytes_to_store);

    // round up to the next multiple of chunk_size
    let bytes_to_store = bytes_to_store.div_ceil(chunk_size) * chunk_size;

    match data_ledger {
        DataLedger::Publish => {
            // Get the latest EMA for pricing calculations from the canonical chain
            let tree = state.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block_entry = canonical
                .last()
                .ok_or(("Empty canonical chain", StatusCode::BAD_REQUEST))?;
            let ema = tree
                .get_ema_snapshot(&last_block_entry.block_hash())
                .ok_or(("EMA snapshot not available", StatusCode::BAD_REQUEST))?;
            // Get the actual block to access its timestamp
            // Convert block timestamp from millis to seconds for hardfork params
            let latest_block_timestamp_secs = last_block_entry.header().timestamp_secs();
            drop(tree);

            // Calculate the actual epochs remaining for the next block based on height
            let tip_height = last_block_entry.height();
            let next_block_height = tip_height + 1;

            let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
                next_block_height,
                state.config.consensus.epoch.num_blocks_in_epoch,
                state.config.consensus.epoch.submit_ledger_epoch_length,
            );

            // Determine pricing EMA for public quotes with special handling in
            // the last quarter of the adjustment interval.
            // - Not in last quarter: use stable public pricing (2 intervals ago)
            // - In last quarter: use the LOWER of the last two EMAs (more conservative in IRYS)
            //   Note: lower USD price -> higher IRYS required
            let price_adjustment_interval = state.config.consensus.ema.price_adjustment_interval;
            let position_in_interval = next_block_height % price_adjustment_interval;
            let last_quarter_start =
                price_adjustment_interval - price_adjustment_interval.div_ceil(4);
            let in_last_quarter = position_in_interval >= last_quarter_start;

            let pricing_ema = if in_last_quarter {
                if ema.ema_price_1_interval_ago.amount < ema.ema_price_2_intervals_ago.amount {
                    ema.ema_price_1_interval_ago
                } else {
                    ema.ema_price_2_intervals_ago
                }
            } else {
                ema.ema_for_public_pricing()
            };

            // Get hardfork params using the latest block's timestamp
            let number_of_ingress_proofs_total = state
                .config
                .number_of_ingress_proofs_total_at(latest_block_timestamp_secs);

            // Calculate term fee using the dynamic epoch count
            let term_fee = calculate_term_fee(
                bytes_to_store,
                epochs_for_storage,
                &state.config.consensus,
                number_of_ingress_proofs_total,
                pricing_ema,
                next_block_height,
            )
            .map_err(|e| {
                (
                    format!("Failed to calculate term fee: {e:?}"),
                    StatusCode::BAD_REQUEST,
                )
            })?;

            tracing::debug!(
                "Fee calculation - bytes: {}, term_fee: {}, inclusion_reward_percent raw: {}, num_ingress_proofs: {}",
                bytes_to_store,
                term_fee,
                state.config.consensus.immediate_tx_inclusion_reward_percent.amount,
                number_of_ingress_proofs_total
            );

            // If the cost calculation fails, return 400 with the error text
            let total_perm_cost = calculate_perm_fee_from_config(
                bytes_to_store,
                &state.config.consensus,
                number_of_ingress_proofs_total,
                pricing_ema,
                term_fee,
                next_block_height,
            )
            .map_err(|e| (format!("{e:?}"), StatusCode::BAD_REQUEST))?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                perm_fee: total_perm_cost.amount,
                term_fee,
                ledger,
                bytes: bytes_to_store,
            }))
        }
        DataLedger::Submit | DataLedger::OneYear | DataLedger::ThirtyDay => {
            // Term ledger pricing — term-fee only, no perm_fee
            let cascade = state.config.consensus.hardforks.cascade.as_ref();

            // OneYear/ThirtyDay require Cascade to be active
            if matches!(data_ledger, DataLedger::OneYear | DataLedger::ThirtyDay) {
                let tree = state.block_tree.read();
                let (canonical, _) = tree.get_canonical_chain();
                let tip_height = canonical
                    .last()
                    .map(irys_domain::BlockTreeEntry::height)
                    .unwrap_or(0);
                drop(tree);
                if !state
                    .config
                    .consensus
                    .hardforks
                    .is_cascade_active(tip_height)
                {
                    return Err((
                        format!(
                            "{:?} ledger not available: Cascade hardfork not active",
                            data_ledger
                        ),
                        StatusCode::BAD_REQUEST,
                    )
                        .into());
                }
            }

            let epoch_length = match data_ledger {
                DataLedger::Submit => state.config.consensus.epoch.submit_ledger_epoch_length,
                DataLedger::OneYear => cascade.map(|c| c.one_year_epoch_length).unwrap_or(365),
                DataLedger::ThirtyDay => cascade.map(|c| c.thirty_day_epoch_length).unwrap_or(30),
                _ => unreachable!(),
            };

            let tree = state.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block_entry = canonical
                .last()
                .ok_or(("Empty canonical chain", StatusCode::BAD_REQUEST))?;
            let ema = tree
                .get_ema_snapshot(&last_block_entry.block_hash())
                .ok_or(("EMA snapshot not available", StatusCode::BAD_REQUEST))?;
            let latest_block_timestamp_secs = last_block_entry.header().timestamp_secs();
            let tip_height = last_block_entry.height();
            drop(tree);

            let next_block_height = tip_height + 1;
            let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
                next_block_height,
                state.config.consensus.epoch.num_blocks_in_epoch,
                epoch_length,
            );

            let price_adjustment_interval = state.config.consensus.ema.price_adjustment_interval;
            let position_in_interval = next_block_height % price_adjustment_interval;
            let last_quarter_start =
                price_adjustment_interval - price_adjustment_interval.div_ceil(4);
            let in_last_quarter = position_in_interval >= last_quarter_start;

            let pricing_ema = if in_last_quarter {
                if ema.ema_price_1_interval_ago.amount < ema.ema_price_2_intervals_ago.amount {
                    ema.ema_price_1_interval_ago
                } else {
                    ema.ema_price_2_intervals_ago
                }
            } else {
                ema.ema_for_public_pricing()
            };

            // Submit uses the full ingress proof replica count (part of perm pipeline).
            // OneYear/ThirtyDay have no ingress proofs — replica count is 1.
            let replica_count = match data_ledger {
                DataLedger::Submit => state
                    .config
                    .number_of_ingress_proofs_total_at(latest_block_timestamp_secs),
                _ => 1,
            };

            let term_fee = calculate_term_fee(
                bytes_to_store,
                epochs_for_storage,
                &state.config.consensus,
                replica_count,
                pricing_ema,
                next_block_height,
            )
            .map_err(|e| {
                (
                    format!("Failed to calculate term fee: {e:?}"),
                    StatusCode::BAD_REQUEST,
                )
            })?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                perm_fee: U256::from(0),
                term_fee,
                ledger,
                bytes: bytes_to_store,
            }))
        }
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

pub async fn get_pledge_price(
    address: Path<IrysAddress>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let user_address = address.into_inner();

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
    address: Path<IrysAddress>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let user_address = address.into_inner();

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
