//! endpoint tests

use crate::{api::price_endpoint_request, utils::IrysNodeTest};
use actix_web::{http::header::ContentType, HttpMessage};
use irys_api_server::routes::price::PriceInfo;
use irys_database::DataLedger;
use irys_types::U256;

#[test_log::test(actix::test)]
async fn pricing_endpoint() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default().start().await;
    let address = format!("http://127.0.0.1:{}", ctx.node_ctx.config.port);
    let data_size_bytes = 1024_u64;
    let expected_price = {
        let cost_per_gb = ctx
            .cfg
            .config
            .annual_cost_per_gb
            .cost_per_replica(
                ctx.cfg.config.safe_minimum_number_of_years,
                ctx.cfg.config.decay_rate,
            )?
            .replica_count(ctx.cfg.config.number_of_ingerss_proofs)?;

        cost_per_gb
            .base_network_fee(
                U256::from(data_size_bytes),
                // node just started up, using genesis ema price
                ctx.cfg.config.genesis_token_price,
            )?
            .add_multiplier(ctx.cfg.config.fee_percentage)?
    };

    // action
    let mut response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), 200);
    assert_eq!(response.content_type(), ContentType::json().to_string());
    let price_info = response.json::<PriceInfo>().await?;
    assert_eq!(
        price_info,
        PriceInfo {
            cost_in_irys: expected_price.amount,
            ledger: 0,
            bytes: data_size_bytes,
        }
    );

    ctx.node_ctx.stop().await;
    Ok(())
}
