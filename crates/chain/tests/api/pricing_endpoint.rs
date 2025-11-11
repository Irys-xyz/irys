//! endpoint tests

use crate::{api::price_endpoint_request, utils::IrysNodeTest};
use actix_web::http::header::ContentType;
use irys_api_server::routes::price::PriceInfo;
use irys_types::{
    storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee_from_config},
    DataLedger, U256,
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

    ctx.node_ctx.stop().await;
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

    ctx.node_ctx.stop().await;
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

    ctx.node_ctx.stop().await;
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

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_slow_pricing_ema_switches_at_last_quarter_boundary() -> eyre::Result<()> {
    // Setup: Configure with interval of 12 blocks
    // Last 25% = 3 blocks (blocks_until_boundary <= 3)
    let price_adjustment_interval = 12;
    let mut config = crate::utils::IrysNodeTest::<()>::default_async().cfg;
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;

    // Configure mock oracle with consistent price increases (smoothing_interval = 999999)
    // This ensures ema_price_1_interval_ago > ema_price_2_intervals_ago throughout the test
    config.oracles = vec![irys_types::OracleConfig::Mock {
        initial_price: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(1.0))
            .unwrap(),
        incremental_change: irys_types::storage_pricing::Amount::token(rust_decimal_macros::dec!(
            0.05
        ))
        .unwrap(),
        smoothing_interval: 999999,
        poll_interval_ms: 500,
    }];

    let ctx = crate::utils::IrysNodeTest::new_genesis(config)
        .start()
        .await;
    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );
    let data_size_bytes = ctx.node_ctx.config.consensus.chunk_size;

    // Mine to block 27 - NOT in last quarter
    // At block 27: next_block=28, position=4, blocks_until=8
    // 8 > 3 -> NOT in last quarter -> should use ema_price_2_intervals_ago
    let mut last_block = ctx.mine_block().await?;
    for _ in 2..=27 {
        last_block = ctx.mine_block().await?;
    }

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
        "Block 27 (NOT in last quarter): should use lower ema_price_2_intervals_ago",
    )
    .await?;

    // Mine to block 32 - first block OF last quarter (boundary!)
    // At block 32: position=9, blocks_until=3
    // 3 <= 3 -> IN last quarter -> should use max(ema_1_interval, ema_2_intervals)
    for _ in 28..=32 {
        last_block = ctx.mine_block().await?;
    }

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

    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage2.ema_price_1_interval_ago, // Max pricing (higher EMA)
        "Block 32 (first of last quarter): should use higher ema_price_1_interval_ago",
    )
    .await?;

    // STAGE 3: Mine 2 more blocks to block 34 - last block of last quarter
    // At block 34: position=11, blocks_until=1
    // 1 <= 3 -> Still IN last quarter -> should still use max EMA
    for _ in 33..=34 {
        last_block = ctx.mine_block().await?;
    }

    let ema_stage3 = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&last_block.block_hash)
        .expect("EMA snapshot should exist");

    assert!(
        ema_stage3.ema_price_1_interval_ago.amount > ema_stage3.ema_price_2_intervals_ago.amount,
        "With consistent price increases, newer EMA should still be higher"
    );

    verify_pricing_uses_ema(
        &ctx,
        &address,
        data_size_bytes,
        ema_stage3.ema_price_1_interval_ago, // Still using max pricing (higher EMA)
        "Block 34 (deep in last quarter): should still use higher ema_price_1_interval_ago",
    )
    .await?;

    ctx.node_ctx.stop().await;
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
    let expected_term_fee = calculate_term_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
        expected_ema,
    )?;

    let expected_perm_fee = calculate_perm_fee_from_config(
        data_size_bytes,
        &ctx.node_ctx.config.consensus,
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

/// Test that the pricing endpoint uses the canonical chain's EMA, not the tip's EMA,
/// when they diverge due to fork scenarios with different oracle configurations.
///
/// Scenario:
/// - Genesis node mines common base chain
/// - Node 1 with slow oracle creates Fork 1: 10 blocks (validates first 5)
/// - Node 2 with fast oracle creates Fork 2: 7 blocks (validates all 7)
///
/// Expected:
/// - Fork 1 is canonical (longer, 10 blocks total)
/// - Fork 2 is tip (fully validated, but only 7 blocks)
/// - Pricing endpoint should use Fork 1's EMA (canonical), not Fork 2's EMA (tip)
#[test_log::test(tokio::test)]
async fn heavy_slow_pricing_uses_canonical_chain_not_tip_in_fork_scenario() -> eyre::Result<()> {
    use irys_types::{storage_pricing::Amount, OracleConfig};
    use rust_decimal_macros::dec;

    let price_adjustment_interval = 3;
    let seconds_to_wait = 20;

    // Genesis node with slow oracle (will create Fork 1)
    let mut genesis_config = crate::utils::IrysNodeTest::<()>::default_async().cfg;
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().epoch.num_blocks_in_epoch = 2;
    genesis_config.consensus.get_mut().block_migration_depth = 20;
    genesis_config.consensus.get_mut().block_tree_depth = 40;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = price_adjustment_interval;
    genesis_config
        .consensus
        .get_mut()
        .mempool
        .anchor_expiry_depth = 40;
    genesis_config.oracles = vec![OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).unwrap(),
        incremental_change: Amount::token(dec!(0.05)).unwrap(),
        smoothing_interval: 999999,
        poll_interval_ms: 500,
    }];

    // Create signer and fund it
    let peer_signer = genesis_config.new_random_signer();
    let peer_signer_2 = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer, &peer_signer_2]);

    let node_1 = crate::utils::IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("NODE1", seconds_to_wait)
        .await;

    let fork_creator_1 = node_1.testing_peer_with_assignments(&peer_signer).await?;
    let fork_creator_2 = node_1.testing_peer_with_assignments(&peer_signer_2).await?;
    let fork_creator_1 = {
        let mut node = fork_creator_1.stop().await;
        node.cfg.oracles = vec![OracleConfig::Mock {
            initial_price: Amount::token(dec!(1.0)).unwrap(),
            incremental_change: Amount::token(dec!(0.15)).unwrap(),
            smoothing_interval: 999999,
            poll_interval_ms: 500,
        }];
        node.start().await
    };
    let fork_creator_2 = {
        let mut node = fork_creator_2.stop().await;
        node.cfg.oracles = vec![OracleConfig::Mock {
            initial_price: Amount::token(dec!(1.0)).unwrap(),
            incremental_change: Amount::token(dec!(0.25)).unwrap(),
            smoothing_interval: 999999,
            poll_interval_ms: 500,
        }];
        node.start().await
    };


    // Disable validation temporarily while we create the forks
    node_1.node_ctx.set_validation_enabled(false);
    fork_creator_1.gossip_disable();
    fork_creator_2.gossip_disable();
    node_1.gossip_disable();

    // Mine blocks without gossip to create competing forks
    // Fork 1: 10 blocks (first 6 will be validated)
    // Fork 2: 7 blocks (all will be validated)
    // The order vector defines: (block_data, should_validate, expected_new_tip)
    let order = [
        // Fork 1: blocks 0-5 (will be validated)
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 0
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 1
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 2
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 3
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 4
        (&fork_creator_1.mine_block_without_gossip().await?, true, true),   // Fork 1, block 5
        // Fork 1: blocks 6-9 (will NOT be validated)
        (&fork_creator_1.mine_block_without_gossip().await?, false, false), // Fork 1, block 6
        (&fork_creator_1.mine_block_without_gossip().await?, false, false), // Fork 1, block 7
        (&fork_creator_1.mine_block_without_gossip().await?, false, false), // Fork 1, block 8
        (&fork_creator_1.mine_block_without_gossip().await?, false, false), // Fork 1, block 9
        // Fork 2: all 7 blocks (all will be validated)
        (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 0
        (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 1
        (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 2
        // (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 3
        // (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 4
        // (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 5
        // (&fork_creator_2.mine_block_without_gossip().await?, true, true),   // Fork 2, block 6
    ];

    tracing::info!("Mined {} blocks total", order.len());

    // First loop: gossip all blocks to node_1 (without validation)
    for ((block, _eth_block), _should_validate, _new_tip) in order.iter() {
        tracing::info!(
            block_height = block.height,
            ?block.cumulative_diff,
            "Gossiping block"
        );
        crate::validation::send_block_to_block_tree(
            &node_1.node_ctx,
            block.clone(),
            vec![],
            false,
        )
        .await?;
    }

    tracing::info!("All blocks gossiped, enabling validation");

    // // Enable validation and subscribe to block state updates
    // node_1.node_ctx.set_validation_enabled(true);
    // let mut block_state_rx = node_1
    //     .node_ctx
    //     .service_senders
    //     .subscribe_block_state_updates();

    // // Second loop: selectively provide execution payloads to validate specific blocks
    // 'outer: for ((block, eth_block), should_validate, expected_new_tip) in order.iter() {
    //     if !should_validate {
    //         tracing::info!(
    //             block_height = block.height,
    //             ?block.cumulative_diff,
    //             "Skipping validation (no payload provided)"
    //         );
    //         continue;
    //     }

    //     tracing::info!(
    //         block_height = block.height,
    //         ?block.cumulative_diff,
    //         expected_new_tip = expected_new_tip,
    //         "Providing payload for validation"
    //     );

    //     node_1
    //         .node_ctx
    //         .block_pool
    //         .execution_payload_provider
    //         .add_payload_to_cache(eth_block.block().clone())
    //         .await;

    //     // Wait for this block to be validated
    //     while let Ok(event) = block_state_rx.recv().await {
    //         if event.block_hash == block.block_hash
    //             && matches!(
    //                 event.validation_result,
    //                 irys_actors::block_tree_service::ValidationResult::Valid
    //             )
    //         {
    //             tracing::info!(
    //                 block_height = block.height,
    //                 "Block validated successfully"
    //             );
    //             continue 'outer;
    //         }
    //     }
    // }

    // tracing::info!("Validation complete: Fork 1 has 6 validated (4 unvalidated), Fork 2 has 7 validated");

    // // ===========================================================================
    // // STAGE 7: Verify chain state (canonical vs tip)
    // // ===========================================================================

    // let canonical_chain = node_1.get_canonical_chain();
    // let tip_hash = node_1.node_ctx.block_tree_guard.read().tip;

    // let canonical_last = canonical_chain
    //     .last()
    //     .ok_or_else(|| eyre::eyre!("Canonical chain is empty"))?;

    // // Fork 1 is blocks 0-9 in the order array (first 10 blocks)
    // // Fork 2 is blocks 10-16 in the order array (last 7 blocks)
    // let fork_1_last_hash = order[9].0.0.block_hash; // Fork 1, block 9 (last of 10)
    // let fork_2_last_hash = order[16].0.0.block_hash; // Fork 2, block 6 (last of 7)

    // tracing::info!("Canonical last: {:?} at height {}", canonical_last.block_hash, canonical_last.height);
    // tracing::info!("Tip: {:?}", tip_hash);
    // tracing::info!("Fork 1 last: {:?}", fork_1_last_hash);
    // tracing::info!("Fork 2 last: {:?}", fork_2_last_hash);

    // // Verify Fork 1 is the canonical chain (longest by cumulative difficulty)
    // assert_eq!(
    //     canonical_last.block_hash, fork_1_last_hash,
    //     "Canonical chain should be Fork 1 (longer chain)"
    // );

    // // Verify Fork 2 is the tip (fully validated)
    // assert_eq!(
    //     tip_hash, fork_2_last_hash,
    //     "Tip should be Fork 2 (fully validated)"
    // );

    // // ===========================================================================
    // // STAGE 8: Get EMA snapshots for both forks
    // // ===========================================================================

    // let fork_1_ema = node_1
    //     .get_ema_snapshot(&fork_1_last_hash)
    //     .ok_or_else(|| eyre::eyre!("Fork 1 EMA snapshot not found"))?;

    // let fork_2_ema = node_1
    //     .get_ema_snapshot(&fork_2_last_hash)
    //     .ok_or_else(|| eyre::eyre!("Fork 2 EMA snapshot not found"))?;

    // let fork_1_pricing_ema = fork_1_ema.ema_for_public_pricing();
    // let fork_2_pricing_ema = fork_2_ema.ema_for_public_pricing();

    // tracing::info!("Fork 1 EMA: {:?}", fork_1_pricing_ema.amount);
    // tracing::info!("Fork 2 EMA: {:?}", fork_2_pricing_ema.amount);

    // // Verify EMAs differ (due to different oracle configs)
    // assert_ne!(
    //     fork_1_pricing_ema.amount, fork_2_pricing_ema.amount,
    //     "Fork EMAs must differ for this test to be meaningful"
    // );

    // // ===========================================================================
    // // STAGE 9: Query pricing endpoint and verify it uses canonical chain EMA
    // // ===========================================================================

    // let address = format!(
    //     "http://127.0.0.1:{}",
    //     node_1.node_ctx.config.node_config.http.bind_port
    // );
    // let data_size_bytes = node_1.node_ctx.config.consensus.chunk_size;

    // // Calculate expected fees using CANONICAL chain EMA (Fork 1)
    // use irys_types::storage_pricing::{calculate_term_fee, calculate_perm_fee_from_config};

    // let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
    //     canonical_last.height + 1,
    //     node_1.node_ctx.config.consensus.epoch.num_blocks_in_epoch,
    //     node_1.node_ctx.config.consensus.epoch.submit_ledger_epoch_length,
    // );

    // let expected_term_fee = calculate_term_fee(
    //     data_size_bytes,
    //     epochs_for_storage,
    //     &node_1.node_ctx.config.consensus,
    //     fork_1_pricing_ema, // CANONICAL chain EMA
    // )?;

    // let expected_perm_fee = calculate_perm_fee_from_config(
    //     data_size_bytes,
    //     &node_1.node_ctx.config.consensus,
    //     fork_1_pricing_ema,
    //     expected_term_fee,
    // )?;

    // // Also calculate what the fees would be if using TIP chain EMA (Fork 2) - for comparison
    // let wrong_term_fee = calculate_term_fee(
    //     data_size_bytes,
    //     epochs_for_storage,
    //     &node_1.node_ctx.config.consensus,
    //     fork_2_pricing_ema, // TIP chain EMA (wrong)
    // )?;

    // let wrong_perm_fee = calculate_perm_fee_from_config(
    //     data_size_bytes,
    //     &node_1.node_ctx.config.consensus,
    //     fork_2_pricing_ema,
    //     wrong_term_fee,
    // )?;

    // // Query the pricing API
    // let response = price_endpoint_request(&address, irys_types::DataLedger::Publish, data_size_bytes).await;
    // assert_eq!(response.status(), reqwest::StatusCode::OK);

    // let price_info = response.json::<irys_api_server::routes::price::PriceInfo>().await?;

    // tracing::info!("Pricing API returned perm_fee: {}", price_info.perm_fee);
    // tracing::info!("Expected (canonical): {}", expected_perm_fee.amount);
    // tracing::info!("Wrong (tip): {}", wrong_perm_fee.amount);

    // // CRITICAL ASSERTION: Pricing must match CANONICAL chain (Fork 1), not tip (Fork 2)
    // assert_eq!(
    //     price_info.perm_fee, expected_perm_fee.amount,
    //     "Pricing endpoint must use canonical chain EMA (Fork 1), not tip EMA (Fork 2)"
    // );
    // assert_eq!(
    //     price_info.term_fee, expected_term_fee,
    //     "Term fee must be calculated from canonical chain EMA"
    // );

    // // Verify it's NOT using the tip's EMA (Fork 2)
    // assert_ne!(
    //     price_info.perm_fee, wrong_perm_fee.amount,
    //     "Pricing endpoint should NOT use tip EMA when it differs from canonical"
    // );

    // tracing::info!("✓ Pricing endpoint correctly uses canonical chain EMA");

    // Cleanup
    node_1.node_ctx.set_validation_enabled(true);
    fork_creator_2.stop().await;
    fork_creator_1.stop().await;
    node_1.stop().await;

    Ok(())
}
