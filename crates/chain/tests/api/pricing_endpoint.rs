//! endpoint tests

use crate::{api::price_endpoint_request, utils::IrysNodeTest};
use actix_web::http::header::ContentType;
use irys_api_server::routes::price::PriceInfo;
use irys_types::{
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee_from_config},
    DataLedger, HardforkParams, U256,
};

#[test_log::test(tokio::test)]
async fn heavy_pricing_endpoint_a_lot_of_data() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size * 5;

    // Calculate the expected term fee
    let hardfork_params = HardforkParams::default();
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        expected_term_fee,
    )?;

    // action
    let response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        &ContentType::json().to_string()
    );
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

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
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
            .replica_count(ctx.node_ctx.config.consensus.hardforks.frontier.number_of_ingress_proofs_total)?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // the original data_size_bytes is too small to fill up a whole chunk
            U256::from(ctx.node_ctx.config.consensus.chunk_size),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee
    let hardfork_params = HardforkParams::default();
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size, // small data rounds up to chunk_size
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        ctx.node_ctx.config.consensus.hardforks.frontier.number_of_ingress_proofs_total,
        ctx.node_ctx
            .config
            .consensus
            .immediate_tx_inclusion_reward_percent,
    )?;

    // action
    let response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        &ContentType::json().to_string()
    );
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

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_pricing_endpoint_submit_ledger_rejected() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // action - try to get price for Submit ledger
    let response = price_endpoint_request(&address, DataLedger::Submit, data_size_bytes).await;

    // assert - should return 400 error for Submit ledger
    assert_eq!(response.status(), 400);
    let body_str = response.text().await?;
    assert!(body_str.contains("Term ledger not supported"));

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
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
            .replica_count(ctx.node_ctx.config.consensus.hardforks.frontier.number_of_ingress_proofs_total)?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // round to the chunk size boundary
            U256::from(ctx.node_ctx.config.consensus.chunk_size * 2),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee
    let hardfork_params = HardforkParams::default();
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size * 2, // data rounds up to 2 chunks
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        ctx.node_ctx.config.consensus.hardforks.frontier.number_of_ingress_proofs_total,
        ctx.node_ctx
            .config
            .consensus
            .immediate_tx_inclusion_reward_percent,
    )?;

    // action
    let response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;

    // assert
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        &ContentType::json().to_string()
    );
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

    ctx.stop().await;
    Ok(())
}

// Ensure correct behavior when EMA trend is increasing.
// We expect the pricing endpoint to choose the LOWER of the two EMAs
// during the last quarter. This yields lower IRYS amounts (higher USD price -> lower IRYS).
#[test_log::test(tokio::test)]
async fn heavy_slow_pricing_ema_switches_at_last_quarter_boundary() -> eyre::Result<()> {
    // Setup: Configure with interval of 4 blocks
    // Last 25% = 1 block (position_in_interval == 3)
    let price_adjustment_interval = 4;
    let mut config = crate::utils::IrysNodeTest::<()>::default_async().cfg;
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;

    // Configure mock oracle with consistent price increases (smoothing_interval = 999999).
    // This ensures ema_price_1_interval_ago > ema_price_2_intervals_ago throughout the test.
    config.oracles = vec![irys_types::OracleConfig::Mock {
        initial_price: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(1.0))
            .unwrap(),
        incremental_change: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(
            0.05
        ))
        .unwrap(),
        smoothing_interval: 999999,
        initial_direction_up: true,
        poll_interval_ms: 500,
    }];

    // Fund a test signer so we can submit a tx using the quoted price
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let ctx = crate::utils::IrysNodeTest::new_genesis(config)
        .start()
        .await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // mine 2 full intervals so public pricing diverges from genesis
    ctx.mine_two_ema_intervals(price_adjustment_interval)
        .await?;

    // Stage 1: NOT in last quarter (interval=4)
    let mut last_block = ctx
        .ema_mine_until_next_not_in_last_quarter(price_adjustment_interval)
        .await?;

    let ema_stage1 = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&last_block.block_hash)
        .expect("EMA snapshot should exist");

    assert!(
        ema_stage1.ema_price_1_interval_ago.amount > ema_stage1.ema_price_2_intervals_ago.amount,
        "With consistent price increases, newer EMA should be higher"
    );

    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage1.ema_price_2_intervals_ago, // Standard pricing (lower EMA)
        "(NOT in last quarter): should use lower ema_price_2_intervals_ago",
    )
    .await?;

    // Stage 2: First block of last quarter (interval=4)
    last_block = ctx
        .ema_mine_until_next_in_last_quarter(price_adjustment_interval)
        .await?;

    let ema_stage2 = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&last_block.block_hash)
        .expect("EMA snapshot should exist");

    assert!(
        ema_stage2.ema_price_1_interval_ago.amount > ema_stage2.ema_price_2_intervals_ago.amount,
        "With consistent price increases, newer EMA should be higher"
    );

    // Last quarter uses the LOWER of the two EMAs for public pricing
    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage2.ema_price_2_intervals_ago,
        "Last quarter: should use lower-of-two (ema_price_2_intervals_ago)",
    )
    .await?;

    let data = vec![1_u8; 1024];
    ctx.post_publish_data_tx(&signer, data)
        .await
        .expect("Tx using API price at last-quarter boundary was not accepted");

    ctx.stop().await;
    Ok(())
}

// Same boundary test, but ensure behavior when EMA trend is decreasing.
// We still expect the pricing endpoint to choose the LOWER of the two EMAs
// during the last quarter. This yields higher IRYS amounts (lower USD price -> higher IRYS).
#[test_log::test(tokio::test)]
async fn heavy_slow_pricing_ema_switches_at_last_quarter_boundary_decreasing() -> eyre::Result<()> {
    // Configure with interval of 4 blocks
    let price_adjustment_interval = 4;
    let mut config = crate::utils::IrysNodeTest::<()>::default_async().cfg;
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;

    // Configure mock oracle with an initially decreasing trend
    config.oracles = vec![irys_types::OracleConfig::Mock {
        initial_price: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(1.0))
            .unwrap(),
        incremental_change: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(
            0.05
        ))
        .unwrap(),
        smoothing_interval: 999999,
        initial_direction_up: false,
        poll_interval_ms: 500,
    }];

    // Fund a test signer so we can submit a tx using the quoted price
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);

    let ctx = crate::utils::IrysNodeTest::new_genesis(config)
        .start()
        .await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // mine 2 full intervals so public pricing diverges from genesis
    ctx.mine_two_ema_intervals(price_adjustment_interval)
        .await?;

    // Stage 1: NOT in last quarter (interval=4)
    let mut last_block = ctx
        .ema_mine_until_next_not_in_last_quarter(price_adjustment_interval)
        .await?;

    let ema_stage1 = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&last_block.block_hash)
        .expect("EMA snapshot should exist");

    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage1.ema_price_2_intervals_ago,
        "(NOT in last quarter): should use lower ema_price_2_intervals_ago",
    )
    .await?;

    // Stage 2: First block of last quarter (interval=4)
    last_block = ctx
        .ema_mine_until_next_in_last_quarter(price_adjustment_interval)
        .await?;
    let ema_stage2 = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&last_block.block_hash)
        .expect("EMA snapshot should exist");

    // Ensure we're in the decreasing scenario
    assert!(
        ema_stage2.ema_price_1_interval_ago.amount < ema_stage2.ema_price_2_intervals_ago.amount,
        "Expected newer EMA to be lower (decreasing trend)"
    );

    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage2.ema_price_1_interval_ago,
        "Last quarter: should use lower-of-two (ema_price_1_interval_ago)",
    )
    .await?;

    // Also submit a tx priced via the API at the last-quarter boundary and ensure acceptance
    let data = vec![2_u8; 1024];
    ctx.post_publish_data_tx(&signer, data)
        .await
        .expect("Tx using API price in decreasing last-quarter scenario was not accepted");
    ctx.stop().await;
    Ok(())
}

/// Helper function to verify pricing API uses the expected EMA value
async fn verify_pricing_uses_ema(
    ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: &str,
    data_size_bytes: u64,
    expected_ema: irys_types::IrysTokenPrice,
    block_description: &str,
) -> eyre::Result<()> {
    // Calculate expected fees using the provided EMA
    let hardfork_params = HardforkParams::default();
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        expected_ema,
    )?;

    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        &hardfork_params,
        expected_ema,
        expected_term_fee,
    )?;

    // Query the pricing API
    let response = price_endpoint_request(address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let price_info = response.json::<PriceInfo>().await?;

    // Validate the API response matches our expected calculation
    assert_eq!(
        price_info.perm_fee, expected_perm_fee.amount,
        "{} - perm_fee mismatch",
        block_description
    );
    assert_eq!(
        price_info.term_fee, expected_term_fee,
        "{} - term_fee mismatch",
        block_description
    );

    Ok(())
}
