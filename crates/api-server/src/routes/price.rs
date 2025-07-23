use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use eyre::OptionExt as _;
use irys_actors::mempool_service::{AtomicMempoolState, MempoolServiceMessage};
use irys_types::{
    storage_pricing::{
        phantoms::{Irys, NetworkFee},
        Amount,
    },
    Address, CommitmentType, DataLedger, U256,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr as _;
use tokio::sync::oneshot;

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

/// Get the current pledge count for a user from the canonical blockchain state
async fn get_canonical_pledge_count(state: &web::Data<ApiState>, user_address: &Address) -> usize {
    let commitment_snapshot = state.block_tree.read().canonical_commitment_snapshot();
    commitment_snapshot
        .commitments
        .get(user_address)
        .map(|miner_commitments| miner_commitments.pledges.len())
        .unwrap_or(0)
}

/// Query the mempool state to count pending commitment transactions of a specific type
async fn count_pending_commitments(
    state: &web::Data<ApiState>,
    user_address: &Address,
    commitment_type: CommitmentType,
) -> Result<usize, actix_web::Error> {
    // Query mempool state
    let (tx, rx) = oneshot::channel();
    state
        .mempool_service
        .send(MempoolServiceMessage::GetState(tx))
        .map_err(|_| ErrorBadRequest("Failed to query mempool state"))?;

    let mempool_state: AtomicMempoolState = rx
        .await
        .map_err(|_| ErrorBadRequest("Failed to receive mempool state"))?;

    // Count pending transactions of the specified type
    let count = {
        let mempool = mempool_state.read().await;
        mempool
            .valid_commitment_tx
            .get(user_address)
            .map(|txs| {
                txs.iter()
                    .filter(|tx| tx.commitment_type == commitment_type)
                    .count()
            })
            .unwrap_or(0)
    };

    Ok(count)
}

/// Calculate the pledge value based on the pledge count and decay rate
fn calculate_pledge_value(
    base_value: Amount<Irys>,
    decay_rate: Amount<irys_types::storage_pricing::phantoms::Percentage>,
    pledge_count: usize,
) -> U256 {
    base_value
        .apply_pledge_decay(pledge_count, decay_rate)
        .map(|a| a.amount)
        .unwrap_or(base_value.amount)
}

pub async fn get_pledge_price(
    path: Path<String>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let user_address_str = path.into_inner();
    let user_address = parse_user_address(&user_address_str)?;

    // Get the base pledge count from canonical state
    let mut pledge_count = get_canonical_pledge_count(&state, &user_address).await;

    // Count pending pledge transactions
    let pending_pledges =
        count_pending_commitments(&state, &user_address, CommitmentType::Pledge).await?;

    // Add pending pledges to get the effective pledge count
    // This ensures the next pledge price accounts for pending pledges
    pledge_count = pledge_count.saturating_add(pending_pledges);

    // Calculate the pledge value with decay
    let pledge_value = calculate_pledge_value(
        state.config.consensus.pledge_base_value,
        state.config.consensus.pledge_decay,
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

    // Get the base pledge count from canonical state
    let mut pledge_count = get_canonical_pledge_count(&state, &user_address).await;

    // Count pending unpledge transactions
    let pending_unpledges =
        count_pending_commitments(&state, &user_address, CommitmentType::Unpledge).await?;

    // Adjust pledge count by subtracting pending unpledges
    // This ensures we calculate the refund for the correct pledge
    pledge_count = pledge_count.saturating_sub(pending_unpledges);

    let refund_amount = if pledge_count == 0 {
        U256::from(0)
    } else {
        // Calculate the value of the most recent pledge (count - 1)
        calculate_pledge_value(
            state.config.consensus.pledge_base_value,
            state.config.consensus.pledge_decay,
            pledge_count - 1,
        )
    };

    let commitment_fee = state.config.consensus.mempool.commitment_fee;

    Ok(HttpResponse::Ok().json(CommitmentPriceInfo {
        value: refund_amount, // This is the refund amount
        fee: commitment_fee,
        user_address: Some(user_address),
    }))
}
