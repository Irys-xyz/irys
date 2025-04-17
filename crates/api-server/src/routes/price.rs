use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use irys_actors::ema_service::EmaServiceMessage;
use irys_database::DataLedger;
use irys_types::{
    storage_pricing::{
        phantoms::{Irys, NetworkFee},
        Amount,
    },
    U256,
};
use serde::{Deserialize, Serialize};

use crate::ApiState;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct PriceInfo {
    pub cost_in_irys: U256,
    pub ledger: u32,
    pub bytes: u64,
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

    // enforece that the requested size is at least equal to a single chunk
    let bytes_to_store = std::cmp::max(chunk_size, bytes_to_store);

    // round up to the next multiple of chunk_size
    let bytes_to_store = bytes_to_store.div_ceil(chunk_size) * chunk_size;

    match data_ledger {
        DataLedger::Publish => {
            // If the cost calculation fails, return 400 with the error text
            let perm_storage_price = cost_of_perm_storage(state, bytes_to_store)
                .await
                .map_err(|e| ErrorBadRequest(format!("{:?}", e)))?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                cost_in_irys: perm_storage_price.amount,
                ledger,
                bytes: bytes_to_store,
            }))
        }
        DataLedger::Submit => Err(ErrorBadRequest("Term ledeger not supported")),
    }
}

async fn cost_of_perm_storage(
    state: web::Data<ApiState>,
    bytes_to_store: u64,
) -> eyre::Result<Amount<(NetworkFee, Irys)>> {
    // get the latest EMA to use for pricing
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .ema_service
        .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
    let current_ema = rx.await?;

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
        .replica_count(state.config.consensus.number_of_ingerss_proofs)?;

    // calculate the cost of storing the bytes
    let price_with_network_reward = cost_per_gb
        .base_network_fee(U256::from(bytes_to_store), current_ema)?
        .add_multiplier(state.config.node_config.pricing.fee_percentage)?;

    Ok(price_with_network_reward)
}
