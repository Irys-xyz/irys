//! endpoint tests

use crate::{api::price_endpoint_request, utils::IrysNodeTest};
use actix_web::http::header::ContentType;
use irys_api_server::routes::price::PriceInfo;
use irys_types::{
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee_from_config},
    DataLedger, UnixTimestamp, U256,
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

    // Calculate the expected term fee using hardfork params from config to match API behavior
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        expected_term_fee,
        UnixTimestamp::from_secs(0),
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
async fn pricing_endpoint_small_data() -> eyre::Result<()> {
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
            .replica_count(
                ctx.node_ctx
                    .config
                    .consensus
                    .hardforks
                    .frontier
                    .number_of_ingress_proofs_total,
            )?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // the original data_size_bytes is too small to fill up a whole chunk
            U256::from(ctx.node_ctx.config.consensus.chunk_size),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee using hardfork params from config to match API behavior
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size, // small data rounds up to chunk_size
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        number_of_ingress_proofs_total,
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
async fn pricing_endpoint_submit_ledger_rejected() -> eyre::Result<()> {
    // setup
    let ctx = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // action - get price for Submit ledger (should be rejected as non-user-targetable)
    let response = price_endpoint_request(&address, DataLedger::Submit, data_size_bytes).await;

    // assert - Submit is not user-targetable, should return 400
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn pricing_endpoint_round_data_chunk_up() -> eyre::Result<()> {
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
            .replica_count(
                ctx.node_ctx
                    .config
                    .consensus
                    .hardforks
                    .frontier
                    .number_of_ingress_proofs_total,
            )?;

        cost_per_chunk_duration_adjusted.base_network_fee(
            // round to the chunk size boundary
            U256::from(ctx.node_ctx.config.consensus.chunk_size * 2),
            ctx.node_ctx.config.consensus.chunk_size,
            // node just started up, using genesis ema price
            ctx.node_ctx.config.consensus.genesis.genesis_price,
        )?
    };

    // Calculate the expected term fee using hardfork params from config to match API behavior
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        ctx.node_ctx.config.consensus.chunk_size * 2, // data rounds up to 2 chunks
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;

    // Calculate expected perm_fee using the same method as the API
    let expected_perm_fee = expected_base_fee.add_ingress_proof_rewards(
        expected_term_fee,
        number_of_ingress_proofs_total,
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
async fn heavy_pricing_ema_switches_at_last_quarter_boundary() -> eyre::Result<()> {
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

/// Test that verifies pricing changes when a hardfork activates after mining some blocks.
/// A single node is configured with a hardfork that activates ~5 seconds after genesis.
#[test_log::test(tokio::test)]
async fn heavy_pricing_endpoint_hardfork_changes_ingress_proofs() -> eyre::Result<()> {
    use irys_types::hardfork_config::{FrontierParams, IrysHardforkConfig, NextNameTBD};

    // Define our ingress proof values
    const FRONTIER_PROOFS: u64 = 2;
    const HARDFORK_PROOFS: u64 = 8;

    // Calculate hardfork activation: current time + 5 seconds
    // This ensures the hardfork is NOT active at genesis but activates after mining a few blocks
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let hardfork_activation = now_secs + 5;

    // Configure the node with hardfork that activates in 5 seconds
    let mut config = crate::utils::IrysNodeTest::<()>::default_async().cfg;
    config.consensus.get_mut().hardforks = IrysHardforkConfig {
        frontier: FrontierParams {
            number_of_ingress_proofs_total: FRONTIER_PROOFS,
            number_of_ingress_proofs_from_assignees: 0,
        },
        next_name_tbd: Some(NextNameTBD {
            activation_timestamp: UnixTimestamp::from_secs(hardfork_activation),
            number_of_ingress_proofs_total: HARDFORK_PROOFS,
            number_of_ingress_proofs_from_assignees: 0,
        }),
        aurora: None,
        borealis: None,
        cascade: None,
    };

    let ctx = crate::utils::IrysNodeTest::new_genesis(config)
        .start()
        .await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // Verify hardfork is NOT active at genesis
    let genesis_block = ctx.get_block_by_height(0).await?;
    let genesis_timestamp_secs = genesis_block.timestamp_secs();
    assert!(
        genesis_timestamp_secs.as_secs() < hardfork_activation,
        "Genesis timestamp ({}) should be before hardfork activation ({})",
        genesis_timestamp_secs.as_secs(),
        hardfork_activation
    );
    let proofs_at_genesis = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(genesis_timestamp_secs);
    assert_eq!(
        proofs_at_genesis, FRONTIER_PROOFS,
        "Before hardfork: should use frontier proofs"
    );

    // BEFORE hardfork: Query pricing (should use frontier proofs)
    let response_before =
        price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response_before.status(), reqwest::StatusCode::OK);
    let price_before = response_before.json::<PriceInfo>().await?;

    // Calculate expected fees with frontier params
    let expected_term_fee_before = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        FRONTIER_PROOFS,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;
    let expected_perm_fee_before = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        FRONTIER_PROOFS,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        expected_term_fee_before,
        UnixTimestamp::from_secs(0),
    )?;

    assert_eq!(
        price_before.perm_fee, expected_perm_fee_before.amount,
        "Before hardfork: perm_fee should match expected with {} ingress proofs",
        FRONTIER_PROOFS
    );

    // Mine blocks until timestamp exceeds hardfork activation
    // In testing config, block_time is 1 second, so we need ~5+ blocks
    let post_hardfork_block = loop {
        let block = ctx.mine_block().await?;
        let block_timestamp_secs = block.timestamp_secs().as_secs();
        if block_timestamp_secs >= hardfork_activation {
            break block;
        }
    };

    // Verify hardfork is now active using the block we just mined
    let latest_timestamp_secs = post_hardfork_block.timestamp_secs();
    assert!(
        latest_timestamp_secs.as_secs() >= hardfork_activation,
        "Latest block timestamp ({}) should be >= hardfork activation ({})",
        latest_timestamp_secs.as_secs(),
        hardfork_activation
    );
    let proofs_after_hardfork = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(latest_timestamp_secs);
    assert_eq!(
        proofs_after_hardfork, HARDFORK_PROOFS,
        "After hardfork: should use new hardfork proofs"
    );

    // AFTER hardfork: Query pricing (should use hardfork proofs)
    let response_after =
        price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response_after.status(), reqwest::StatusCode::OK);
    let price_after = response_after.json::<PriceInfo>().await?;

    // Calculate expected fees with hardfork params
    let expected_term_fee_after = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        HARDFORK_PROOFS,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;
    let expected_perm_fee_after = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        HARDFORK_PROOFS,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        expected_term_fee_after,
        UnixTimestamp::from_secs(0),
    )?;

    assert_eq!(
        price_after.perm_fee, expected_perm_fee_after.amount,
        "After hardfork: perm_fee should match expected with {} ingress proofs",
        HARDFORK_PROOFS
    );

    // Assert perm_fee increased - more ingress proofs = higher storage cost
    assert!(
        price_after.perm_fee > price_before.perm_fee,
        "perm_fee should increase after hardfork: before={}, after={}",
        price_before.perm_fee,
        price_after.perm_fee
    );

    ctx.stop().await;
    Ok(())
}

// Same boundary test, but ensure behavior when EMA trend is decreasing.
// We still expect the pricing endpoint to choose the LOWER of the two EMAs
// during the last quarter. This yields higher IRYS amounts (lower USD price -> higher IRYS).
#[test_log::test(tokio::test)]
async fn heavy_pricing_ema_switches_at_last_quarter_boundary_decreasing() -> eyre::Result<()> {
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
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        expected_ema,
        UnixTimestamp::from_secs(0),
    )?;

    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
        expected_ema,
        expected_term_fee,
        UnixTimestamp::from_secs(0),
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

#[test_log::test(tokio::test)]
async fn heavy_cascade_pricing_endpoint_term_ledgers_pre_post_activation() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;

    let num_blocks_in_epoch = 4_u64;

    let config = IrysNodeTest::<()>::default_async()
        .cfg
        .with_consensus(|consensus| {
            consensus.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
            consensus.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(0),
                one_year_epoch_length: 365,
                thirty_day_epoch_length: 30,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
            // Disable minimum term fee so calculated fees reflect actual epoch-length differences
            consensus.minimum_term_fee_usd =
                irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(0)).unwrap();
        });

    let ctx = IrysNodeTest::new_genesis(config).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // Cascade activates from genesis with timestamp 0, so OneYear and ThirtyDay
    // should be available immediately.

    // Post-Cascade: OneYear should return 200 with perm_fee=0, term_fee > 0
    let one_year_after =
        price_endpoint_request(&address, DataLedger::OneYear, data_size_bytes).await;
    assert_eq!(one_year_after.status(), reqwest::StatusCode::OK);
    let one_year_price = one_year_after.json::<PriceInfo>().await?;
    assert_eq!(one_year_price.perm_fee, U256::from(0));
    assert!(one_year_price.term_fee > U256::from(0));

    // Post-Cascade: ThirtyDay should return 200 with perm_fee=0, term_fee > 0
    let thirty_day_after =
        price_endpoint_request(&address, DataLedger::ThirtyDay, data_size_bytes).await;
    assert_eq!(thirty_day_after.status(), reqwest::StatusCode::OK);
    let thirty_day_price = thirty_day_after.json::<PriceInfo>().await?;
    assert_eq!(thirty_day_price.perm_fee, U256::from(0));
    assert!(thirty_day_price.term_fee > U256::from(0));

    // Longer storage = higher fee
    assert!(
        one_year_price.term_fee > thirty_day_price.term_fee,
        "OneYear term_fee should be higher than ThirtyDay term_fee"
    );

    ctx.stop().await;
    Ok(())
}

/// Verify that after Cascade activates, the pricing API returns higher fees
/// based on the Cascade `annual_cost_per_gb` ($0.028) vs the pre-Cascade base ($0.01).
#[test_log::test(tokio::test)]
async fn heavy_cascade_pricing_endpoint_uses_higher_annual_cost() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;
    use irys_types::ledger_expiry::calculate_submit_ledger_expiry;
    use irys_types::storage_pricing::calculate_term_fee;
    use rust_decimal_macros::dec;

    let num_blocks_in_epoch = 4_u64;

    let config = IrysNodeTest::<()>::default_async()
        .cfg
        .with_consensus(|consensus| {
            consensus.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
            // Disable minimum term fee so calculated fees reflect actual cost-basis
            consensus.minimum_term_fee_usd =
                irys_types::storage_pricing::Amount::token(dec!(0)).unwrap();
            consensus.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(0),
                one_year_epoch_length: 365,
                thirty_day_epoch_length: 30,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
        });

    let ctx = IrysNodeTest::new_genesis(config).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    // Use enough data so fees are above the minimum floor
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size * 100;
    let consensus = &ctx.node_ctx.config.consensus;

    // Cascade activates from genesis (timestamp 0), so the Cascade annual_cost_per_gb
    // ($0.028) applies immediately. Query Publish pricing and verify it uses the higher rate.
    let response_after =
        price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response_after.status(), reqwest::StatusCode::OK);
    let price_after = response_after.json::<PriceInfo>().await?;
    let term_fee_after = price_after.term_fee;

    // Verify post-Cascade term_fee matches manual calculation using the same
    // formula as the API route (calculate_submit_ledger_expiry + calculate_term_fee)
    let next_height = ctx.get_canonical_chain_height().await + 1;
    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let epochs_for_storage = calculate_submit_ledger_expiry(
        next_height,
        consensus.epoch.num_blocks_in_epoch,
        consensus.epoch.submit_ledger_epoch_length,
    );
    let expected_term_fee = calculate_term_fee(
        data_size_bytes,
        epochs_for_storage,
        consensus,
        number_of_ingress_proofs_total,
        consensus.genesis.genesis_price,
        UnixTimestamp::from_secs(0),
    )?;
    assert_eq!(
        term_fee_after, expected_term_fee,
        "API term_fee should match manual calculation at post-Cascade height"
    );

    // Perm fee should be non-zero with Cascade active
    assert!(
        price_after.perm_fee > U256::from(0),
        "Post-Cascade perm_fee ({}) should be non-zero",
        price_after.perm_fee,
    );

    ctx.stop().await;
    Ok(())
}

/// Verify the exact transition: fees at height (activation-1) use the base cost,
/// while fees at height (activation) use the Cascade cost.
#[test_log::test(tokio::test)]
async fn heavy_cascade_pricing_transition_at_exact_activation_height() -> eyre::Result<()> {
    use irys_types::hardfork_config::Cascade;
    use irys_types::ledger_expiry::calculate_submit_ledger_expiry;
    use irys_types::storage_pricing::calculate_term_fee;
    use rust_decimal_macros::dec;

    let num_blocks_in_epoch = 2_u64;

    let config = IrysNodeTest::<()>::default_async()
        .cfg
        .with_consensus(|consensus| {
            consensus.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
            consensus.minimum_term_fee_usd =
                irys_types::storage_pricing::Amount::token(dec!(0)).unwrap();
            consensus.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(0),
                one_year_epoch_length: 365,
                thirty_day_epoch_length: 30,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
        });

    let ctx = IrysNodeTest::new_genesis(config).start().await;
    let consensus = &ctx.node_ctx.config.consensus;
    let data_size_bytes = consensus.chunk_size * 100;

    let number_of_ingress_proofs_total = ctx
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

    // Cascade activates from genesis (timestamp 0). Verify the fee at height 1
    // uses the Cascade annual_cost_per_gb rate.
    let compute_api_fee = |height: u64| -> eyre::Result<U256> {
        let epochs = calculate_submit_ledger_expiry(
            height,
            consensus.epoch.num_blocks_in_epoch,
            consensus.epoch.submit_ledger_epoch_length,
        );
        calculate_term_fee(
            data_size_bytes,
            epochs,
            consensus,
            number_of_ingress_proofs_total,
            consensus.genesis.genesis_price,
            UnixTimestamp::from_secs(0),
        )
    };

    let fee_at = compute_api_fee(1)?;
    assert!(
        fee_at > U256::from(0),
        "Fee at height 1 with Cascade active should be non-zero"
    );

    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );

    // The API prices for the *next* block (tip+1). At genesis (tip=0), next block = 1,
    // and Cascade is already active, so the API should return the Cascade fee.
    let response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let price_info = response.json::<PriceInfo>().await?;

    assert_eq!(
        price_info.term_fee, fee_at,
        "API should return Cascade-rate fee when Cascade is active from genesis"
    );

    ctx.stop().await;
    Ok(())
}
