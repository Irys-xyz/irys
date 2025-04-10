use actix_web::{
    error::{ErrorBadRequest, ErrorInternalServerError},
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use irys_actors::ema_service::EmaServiceMessage;
use irys_database::DataLedger;
use irys_types::{storage_pricing::Decimal, U256};
use serde::{Deserialize, Serialize};

use crate::ApiState;

#[derive(Serialize, Deserialize, Debug)]
pub struct PriceInfo {
    cost_in_irys: Decimal,
    ledger: u32,
    bytes: u64,
}

pub async fn get_price(
    path: Path<(u32, u64)>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let (ledger, bytes_to_store) = path.into_inner();

    // Convert ledger to enum, or bail out with an HTTP 400
    let data_ledger =
        DataLedger::try_from(ledger).map_err(|_| ErrorBadRequest("Ledger type not supported"))?;

    match data_ledger {
        DataLedger::Publish => {
            // If the cost calculation fails, return 400 with the error text
            let perm_storage_price = cost_of_publish_ledger(state, bytes_to_store)
                .await
                .map_err(|e| ErrorBadRequest(format!("{:?}", e)))?;

            Ok(HttpResponse::Ok().json(PriceInfo {
                cost_in_irys: perm_storage_price,
                ledger,
                bytes: bytes_to_store,
            }))
        }
        DataLedger::Submit => Ok(HttpResponse::BadRequest().body("term not yet implemented")),
    }
}

async fn cost_of_publish_ledger(
    state: web::Data<ApiState>,
    bytes_to_store: u64,
) -> eyre::Result<Decimal> {
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
        .annual_cost_per_gb
        .cost_per_replica(
            state.config.safe_minimum_number_of_years,
            state.config.decay_rate,
        )?
        .replica_count(state.config.number_of_ingerss_proofs)?;

    // calculate the cost of storing the bytes
    let price_with_network_reward = cost_per_gb
        .base_network_fee(U256::from(bytes_to_store), current_ema)?
        .add_multiplier(state.config.fee_percentage)?
        .token_to_decimal()?;

    Ok(price_with_network_reward)
}
