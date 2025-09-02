use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use irys_types::{
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
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentPriceInfo {
    pub value: U256,
    pub fee: U256,
    pub user_address: Option<Address>,
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

            // Calculate term fee using the dynamic epoch count
            let term_fee = calculate_term_fee(
                bytes_to_store,
                epochs_for_storage,
                &state.config.consensus,
                ema.ema_for_public_pricing(),
            )
            .map_err(|e| ErrorBadRequest(format!("Failed to calculate term fee: {:?}", e)))?;

            // If the cost calculation fails, return 400 with the error text
            let total_perm_cost = calculate_perm_fee_from_config(
                bytes_to_store,
                &state.config.consensus,
                ema.ema_for_public_pricing(),
                term_fee,
            )
            .map_err(|e| ErrorBadRequest(format!("{:?}", e)))?;

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
    }))
}

pub async fn get_unstake_price(state: web::Data<ApiState>) -> ActixResult<HttpResponse> {
    let stake_value = state.config.consensus.stake_value;
    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: stake_value.amount,
        fee: U256::from(commitment_fee),
        user_address: None,
    }))
}

/// Parse and validate a user address from a string
fn parse_user_address(address_str: &str) -> Result<Address, actix_web::Error> {
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
    }))
}
