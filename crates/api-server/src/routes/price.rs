use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use eyre::OptionExt as _;
use irys_types::{
    storage_pricing::{
        phantoms::{Irys, NetworkFee},
        Amount,
    },
    transaction::{CommitmentTransaction, PledgeDataProvider as _},
    Address, DataLedger, U256,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;

use crate::ApiState;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PriceInfo {
    pub cost_in_irys: U256,
    pub ledger: u32,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentPriceInfo {
    pub value: U256,
    pub fee: u64,
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
            // If the cost calculation fails, return 400 with the error text
            let perm_storage_price = cost_of_perm_storage(state, bytes_to_store)
                .map_err(|e| ErrorBadRequest(format!("{:?}", e)))?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                cost_in_irys: perm_storage_price.amount,
                ledger,
                bytes: bytes_to_store,
            }))
        }
        DataLedger::Submit => Err(ErrorBadRequest("Term ledger not supported")),
    }
}

fn cost_of_perm_storage(
    state: web::Data<ApiState>,
    bytes_to_store: u64,
) -> eyre::Result<Amount<(NetworkFee, Irys)>> {
    // get the latest EMA to use for pricing
    let tree = state.block_tree.read();
    let tip = tree.tip;
    let ema = tree
        .get_ema_snapshot(&tip)
        .ok_or_eyre("tip block should still remain in state")?;
    drop(tree);

    // Calculate the cost per GB (take into account replica count & cost per replica)
    // NOTE: this value can be memoised because it is deterministic based on the config
    let cost_per_gb = state
        .config
        .consensus
        .annual_cost_per_gb
        .cost_per_replica(
            state.config.consensus.safe_minimum_number_of_years,
            state.config.consensus.decay_rate,
        )?
        .replica_count(state.config.consensus.number_of_ingress_proofs)?;

    // calculate the cost of storing the bytes
    let price_with_network_reward = cost_per_gb
        .base_network_fee(U256::from(bytes_to_store), ema.ema_for_public_pricing())?
        .add_multiplier(state.config.node_config.pricing.fee_percentage)?;

    Ok(price_with_network_reward)
}

pub async fn get_stake_price(state: web::Data<ApiState>) -> ActixResult<HttpResponse> {
    let stake_value = state.config.consensus.stake_value;
    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: stake_value.amount,
        fee: commitment_fee,
        user_address: None,
    }))
}

pub async fn get_unstake_price(state: web::Data<ApiState>) -> ActixResult<HttpResponse> {
    let stake_value = state.config.consensus.stake_value;
    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: stake_value.amount,
        fee: commitment_fee,
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
        fee: commitment_fee,
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
        fee: commitment_fee,
        user_address: Some(user_address),
    }))
}
