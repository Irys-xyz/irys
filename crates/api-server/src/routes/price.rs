use actix_web::{
    web::{self, Path},
    HttpResponse,
};
use irys_actors::ema_service::EmaServiceMessage;
use irys_database::DataLedger;
use irys_types::{storage_pricing::Decimal, U256};

use crate::ApiState;

pub async fn get_price(
    path: Path<(String, u64)>,
    state: web::Data<ApiState>,
) -> actix_web::Result<HttpResponse> {
    let (ledger, bytes_to_store) = path.into_inner();

    match DataLedger::try_from(ledger.as_str()) {
        Ok(DataLedger::Publish) => match cost_of_publish_ledger(state, bytes_to_store).await {
            Ok(perm_storage_price) => Ok(HttpResponse::Ok().body(perm_storage_price.to_string())),
            Err(e) => Ok(HttpResponse::BadRequest().body(format!("{e:?}"))),
        },
        Ok(DataLedger::Submit) => Ok(HttpResponse::BadRequest().body("term not yet implemented")),
        Err(_) => Ok(HttpResponse::BadRequest().body("Ledger type not supported")),
    }
}

async fn cost_of_publish_ledger(
    state: web::Data<ApiState>,
    bytes_to_store: u64,
) -> eyre::Result<Decimal> {
    // get the latest EMA to use for pricing
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _current_ema = state
        .ema_service
        .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx });
    let current_ema = rx.await?;

    // Calculate the cost per GB
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
        .add_multiplier(state.config.fee_percentage)?;
    let price_with_network_reward = cost_per_gb.token_to_decimal().unwrap();
    Ok(price_with_network_reward)
}
