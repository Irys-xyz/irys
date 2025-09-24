//! endpoint tests

use crate::{api::price_endpoint_request, utils::IrysNodeTest};
use actix_web::{http::header::ContentType, HttpMessage as _};
use irys_api_server::routes::price::PriceInfo;
use irys_types::{
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee_from_config},
    DataLedger, U256,
};

#[test_log::test(actix::test)]
async fn heavy_pricing_endpoint_a_lot_of_data() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size * 5;

    // Calculate the expected term fee
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        expected_term_fee,
    )?;

    // action
    let mut response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), 200);
    assert_eq!(response.content_type(), ContentType::json().to_string());
    let price_info = response.json::<PriceInfo>().await?;
    // Check that perm_fee includes both base fee and ingress rewards
    assert_eq!(price_info.perm_fee, expected_perm_fee.amount);
    // Verify term_fee is calculated correctly
    assert_eq!(price_info.term_fee, expected_term_fee);
    assert_eq!(price_info.ledger, 0);
    assert_eq!(price_info.bytes, data_size_bytes);
    assert!(
        data_size_bytes > ctx.node_ctx.config.consensus.chunk_size,
        "for the test to be accurate, the requested size must be larger to the configs chunk size"
    );

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(actix::test)]
async fn heavy_pricing_endpoint_small_data() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = 4_u64;

    // Calculate the expected base storage fee
    let expected_base_fee = {
        let epochs_for_storage = ctx
            .node_ctx
            .config
            .consensus
            .years_to_epochs(ctx.node_ctx.config.consensus.safe_minimum_number_of_years);
        let cost_per_chunk_per_epoch = ctx.node_ctx.config.consensus.cost_per_chunk_per_epoch()?;
        // Convert annual decay rate to per-epoch
        let epochs_per_year =
            irys_types::U256::from(ctx.node_ctx.config.consensus.epochs_per_year());
        let decay_rate_per_epoch =
            irys_types::storage_pricing::Amount::new(irys_types::storage_pricing::safe_div(
                ctx.node_ctx.config.consensus.decay_rate.amount,
                epochs_per_year,
            )?);
        let cost_per_chunk_duration_adjusted = cost_per_chunk_per_epoch
            .cost_per_replica(epochs_for_storage, decay_rate_per_epoch)?
            .replica_count(ctx.node_ctx.config.consensus.number_of_ingress_proofs_total)?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // the original data_size_bytes is too small to fill up a whole chunk
            U256::from(ctx.node_ctx.config.consensus.chunk_size),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size, // small data rounds up to chunk_size
        &ctx.node_ctx.config.consensus,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        ctx.node_ctx.config.consensus.number_of_ingress_proofs_total,
        ctx.node_ctx
            .config
            .consensus
            .immediate_tx_inclusion_reward_percent,
    )?;

    // action
    let mut response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), 200);
    assert_eq!(response.content_type(), ContentType::json().to_string());
    let price_info = response.json::<PriceInfo>().await?;
    // Check that perm_fee includes both base fee and ingress rewards
    assert_eq!(price_info.perm_fee, expected_perm_fee.amount);
    // Verify term_fee is calculated correctly
    assert_eq!(price_info.term_fee, expected_term_fee);
    assert_eq!(price_info.ledger, 0);
    assert_eq!(price_info.bytes, ctx.node_ctx.config.consensus.chunk_size);
    assert!(
        data_size_bytes < ctx.node_ctx.config.consensus.chunk_size,
        "for the test to be accurate, the requested size must be smaller to the configs chunk size"
    );

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(actix::test)]
async fn heavy_pricing_endpoint_submit_ledger_rejected() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // action - try to get price for Submit ledger
    let mut response = price_endpoint_request(&address, DataLedger::Submit, data_size_bytes).await;

    // assert - should return 400 error for Submit ledger
    assert_eq!(response.status(), 400);
    let body = response.body().await?;
    let body_str = std::str::from_utf8(&body)?;
    assert!(body_str.contains("Term ledger not supported"));

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(actix::test)]
async fn heavy_pricing_endpoint_round_data_chunk_up() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size + 1;

    // Calculate the expected base storage fee
    let expected_base_fee = {
        let epochs_for_storage = ctx
            .node_ctx
            .config
            .consensus
            .years_to_epochs(ctx.node_ctx.config.consensus.safe_minimum_number_of_years);
        let cost_per_chunk_per_epoch = ctx.node_ctx.config.consensus.cost_per_chunk_per_epoch()?;
        // Convert annual decay rate to per-epoch
        let epochs_per_year =
            irys_types::U256::from(ctx.node_ctx.config.consensus.epochs_per_year());
        let decay_rate_per_epoch =
            irys_types::storage_pricing::Amount::new(irys_types::storage_pricing::safe_div(
                ctx.node_ctx.config.consensus.decay_rate.amount,
                epochs_per_year,
            )?);
        let cost_per_chunk_duration_adjusted = cost_per_chunk_per_epoch
            .cost_per_replica(epochs_for_storage, decay_rate_per_epoch)?
            .replica_count(ctx.node_ctx.config.consensus.number_of_ingress_proofs_total)?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // round to the chunk size boundary
            U256::from(ctx.node_ctx.config.consensus.chunk_size * 2),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size * 2, // data rounds up to 2 chunks
        &ctx.node_ctx.config.consensus,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        ctx.node_ctx.config.consensus.number_of_ingress_proofs_total,
        ctx.node_ctx
            .config
            .consensus
            .immediate_tx_inclusion_reward_percent,
    )?;

    // action
    let mut response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), 200);
    assert_eq!(response.content_type(), ContentType::json().to_string());
    let price_info = response.json::<PriceInfo>().await?;
    // Check that perm_fee includes both base fee and ingress rewards
    assert_eq!(price_info.perm_fee, expected_perm_fee.amount);
    // Verify term_fee is calculated correctly
    assert_eq!(price_info.term_fee, expected_term_fee);
    assert_eq!(price_info.ledger, 0);
    assert_eq!(
        price_info.bytes,
        ctx.node_ctx.config.consensus.chunk_size * 2
    );
    assert_ne!(data_size_bytes, ctx.node_ctx.config.consensus.chunk_size, "for the test to be accurate, the requested size must not be equal to the configs chunk size");

    ctx.node_ctx.stop().await;
    Ok(())
}
