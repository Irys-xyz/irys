use std::sync::Arc;

use crate::utils::IrysNodeTest;
use irys_domain::{BlockTreeReadGuard, get_canonical_chain};
use irys_types::{H256, IrysBlockHeader, NodeConfig, OracleConfig, storage_pricing::Amount};
use rust_decimal_macros::dec;

/// When no oracles are configured, block production should still succeed by
/// carrying forward the genesis/parent block's oracle price.
#[test_log::test(tokio::test)]
async fn heavy_test_block_production_without_oracles_uses_parent_price() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.oracles = vec![];

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    let genesis_price = ctx.node_ctx.config.consensus.genesis.genesis_price;

    // Mine 3 blocks — each should carry forward the genesis price since there
    // are no oracles to provide a fresh one.
    for expected_height in 1..=3 {
        ctx.mine_block().await?;
        ctx.wait_until_height(expected_height, 10).await?;
        let header = ctx.get_block_by_height(expected_height).await?;
        assert_eq!(
            header.oracle_irys_price, genesis_price,
            "block {expected_height} should carry forward genesis oracle price when no oracles configured"
        );
    }

    ctx.stop().await;
    Ok(())
}

fn get_block(
    block_tree_read_guard: BlockTreeReadGuard,
    block_hash: H256,
) -> Option<Arc<IrysBlockHeader>> {
    block_tree_read_guard
        .read()
        .get_block(&block_hash)
        .cloned()
        .map(Arc::new)
}

#[test_log::test(tokio::test)]
async fn test_genesis_ema_price_is_respected_for_2_intervals() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // action
    // we start at 1 because the genesis block is already mined
    for expected_height in 1..(price_adjustment_interval * 2) {
        ctx.mine_block().await?;
        ctx.wait_until_height(expected_height, 10).await?;
        let header = ctx.get_block_by_height(expected_height).await?;

        // Get the current EMA price from the block tree
        let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
            .await
            .unwrap();
        let tip_hash = chain.last().unwrap().block_hash();
        let ema_snapshot = ctx
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&tip_hash)
            .expect("EMA snapshot should exist for tip");
        let returned_ema_price = ema_snapshot.ema_for_public_pricing();

        // assert each new block that we mine
        assert_eq!(header.height, expected_height);
        assert_eq!(
            ctx.node_ctx.config.consensus.genesis.genesis_price, returned_ema_price,
            "Genesis price not respected for the expected duration"
        );
        assert_ne!(
            ctx.node_ctx.config.consensus.genesis.genesis_price, header.oracle_irys_price,
            "Expected the header to contain new & unique oracle irys price"
        );
        assert_ne!(
            ctx.node_ctx.config.consensus.genesis.genesis_price, header.ema_irys_price,
            "Expected the header to contain new & unique EMA irys price"
        );
    }

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_genesis_ema_price_updates_after_second_interval() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    //start node with modified config
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    // (oracle price, EMA price)
    let mut registered_prices = vec![(
        ctx.node_ctx.config.consensus.genesis.genesis_price,
        ctx.node_ctx.config.consensus.genesis.genesis_price,
    )];
    // mine 6 blocks
    for expected_height in 1..(price_adjustment_interval * 2) {
        ctx.mine_block().await?;
        ctx.wait_until_height(expected_height, 10).await?;
        let header = ctx.get_block_by_height(expected_height).await?;
        registered_prices.push((header.oracle_irys_price, header.ema_irys_price));
    }

    // action -- mine a new block. This pushes the system to use a new EMA rather than the genesis EMA
    ctx.mine_block().await?;
    ctx.wait_until_height(price_adjustment_interval * 2, 10)
        .await?;
    let header = ctx
        .get_block_by_height(price_adjustment_interval * 2)
        .await?;

    // Get the current EMA price from the block tree
    let tip = ctx.node_ctx.block_tree_guard.read().tip;
    let ema_snapshot = ctx
        .node_ctx
        .block_tree_guard
        .read()
        .get_ema_snapshot(&tip)
        .expect("EMA snapshot should exist for tip");
    let returned_ema_price = ema_snapshot.ema_for_public_pricing();

    // assert
    assert_eq!(
        header.height, 6,
        "expected the 7th block to be mined (height = 6)"
    );
    assert_ne!(
        ctx.node_ctx.config.consensus.genesis.genesis_price, returned_ema_price,
        "After the second interval we no longer use the genesis price"
    );
    assert_eq!(
        registered_prices[2].1, returned_ema_price,
        "expected to use the EMA price registered in the 3rd block"
    );

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_oracle_price_too_high_gets_capped() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let token_price_safe_range = Amount::percentage(dec!(0.1)).unwrap();
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    config.consensus.get_mut().token_price_safe_range = token_price_safe_range;

    config.oracles = vec![OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).unwrap(),
        incremental_change: Amount::token(dec!(1.0)).unwrap(),
        // only change direction after 10 blocks
        smoothing_interval: 10,
        initial_direction_up: true,
        poll_interval_ms: 500,
    }];

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // mine 3 blocks
    let header_1 = ctx.mine_block().await?;
    ctx.wait_until_height(header_1.height, 10).await?;
    let header_2 = ctx.mine_block().await?;
    ctx.wait_until_height(header_2.height, 10).await?;
    let header_3 = ctx.mine_block().await?;
    ctx.wait_until_height(header_3.height, 10).await?;

    // assert that all of the prices are the max allowed ones (guaranteed by the mock oracle reporting inflated values)
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 4, "expected genesis + 3 new blocks");
    let genesis_block =
        get_block(ctx.node_ctx.block_tree_guard.clone(), chain[0].block_hash()).unwrap();
    let mut price_prev = genesis_block.oracle_irys_price;
    for block in [header_1, header_2, header_3] {
        let max_allowed_price = price_prev.add_multiplier(token_price_safe_range).unwrap();
        assert_eq!(
            max_allowed_price, block.oracle_irys_price,
            "the registered price should be the max allowed one"
        );
        price_prev = max_allowed_price;
    }

    ctx.stop().await;
    Ok(())
}

/// From Delta a block's oracle price may move 10% from its parent's, an EMA
/// interval is `Delta::ema_price_adjustment_interval` blocks, and the EMA update
/// takes the median oracle price of the interval. A restarted node rebuilds the
/// same EMA snapshot from its block index.
#[test_log::test(tokio::test)]
async fn heavy_test_delta_ema_uses_interval_median_and_narrow_oracle_bound() -> eyre::Result<()> {
    use irys_types::{IrysTokenPrice, UnixTimestamp, hardfork_config::Delta};

    // setup
    let delta_interval = 3_u64;
    let delta_safe_range = Amount::percentage(dec!(0.1)).unwrap();
    let mut config = NodeConfig::testing();
    {
        let consensus = config.consensus.get_mut();
        // The base bound and interval length must not apply once Delta is active.
        // The base interval still sets the bootstrap: two of them, blocks 0..=9.
        consensus.ema.price_adjustment_interval = 5;
        consensus.token_price_safe_range = Amount::percentage(dec!(1)).unwrap();
        consensus.hardforks.delta = Some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(1),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
            ingress_proof_reward_pool_percent: Delta::default_ingress_proof_reward_pool_percent(),
            token_price_safe_range: delta_safe_range,
            ema_price_adjustment_interval: delta_interval,
        });
    }
    // The mock oracle reports a price far above the bound on every block.
    config.oracles = vec![OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).unwrap(),
        incremental_change: Amount::token(dec!(1.0)).unwrap(),
        smoothing_interval: 100,
        initial_direction_up: true,
        poll_interval_ms: 500,
    }];
    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // action -- mine four Delta intervals; the bootstrap ends at height 9
    let last_height = delta_interval * 4;
    let mut headers = vec![ctx.get_block_by_height(0).await?];
    for expected_height in 1..=last_height {
        ctx.mine_block().await?;
        ctx.wait_until_height(expected_height, 10).await?;
        headers.push(ctx.get_block_by_height(expected_height).await?);
    }

    // assert -- every block reports the highest price the Delta bound allows
    for pair in headers.windows(2) {
        assert_eq!(
            pair[1].oracle_irys_price,
            pair[0]
                .oracle_irys_price
                .add_multiplier(delta_safe_range)
                .unwrap(),
            "block {} must cap its oracle price 10% above its parent's",
            pair[1].height
        );
    }

    // assert -- the first update after the bootstrap, at the end of interval
    // 9..=11, takes the median of its three oracle prices (block 10's) and the
    // EMA of block 9, the last bootstrap block
    let median: IrysTokenPrice = headers[10].oracle_irys_price;
    assert_eq!(
        headers[11].ema_irys_price,
        median.calculate_ema(delta_interval, headers[9].ema_irys_price)?,
        "block 11 must compute its EMA from the median oracle price of blocks 9..=11"
    );

    // assert -- pricing lags two 3-block intervals: block 12 opens the interval
    // that prices with the EMA of block 8
    let snapshot_of = |ctx: &IrysNodeTest<irys_chain::IrysNodeCtx>, block_hash: H256| {
        ctx.node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_hash)
    };
    let tip_snapshot = snapshot_of(&ctx, headers[12].block_hash).expect("tip EMA snapshot");
    assert_eq!(
        tip_snapshot.ema_for_public_pricing(),
        headers[8].ema_irys_price
    );
    assert_eq!(
        tip_snapshot.interval_oracle_prices,
        vec![headers[12].oracle_irys_price]
    );

    // assert -- a restart rebuilds the snapshot of the latest indexed block from
    // chain history; it must equal the one built block by block
    let snapshots_before_restart: Vec<_> = headers
        .iter()
        .map(|header| (header.block_hash, snapshot_of(&ctx, header.block_hash)))
        .collect();
    let ctx = ctx.stop().await.start().await;
    let restarted_tip = ctx.node_ctx.block_tree_guard.read().tip;
    let rebuilt = snapshot_of(&ctx, restarted_tip).expect("EMA snapshot after restart");
    let (_, before_restart) = snapshots_before_restart
        .iter()
        .find(|(block_hash, _)| *block_hash == restarted_tip)
        .expect("the restarted tip was mined before the restart");
    assert_eq!(
        Some(rebuilt.as_ref()),
        before_restart.as_deref(),
        "the snapshot rebuilt from chain history must match the iterative one"
    );

    ctx.stop().await;
    Ok(())
}
