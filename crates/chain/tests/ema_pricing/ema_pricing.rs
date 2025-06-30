use std::sync::Arc;

use crate::utils::{mine_block, IrysNodeTest};
use irys_actors::block_tree_service::{get_canonical_chain, BlockTreeReadGuard};
use irys_types::{storage_pricing::Amount, IrysBlockHeader, NodeConfig, OracleConfig, H256};
use rust_decimal_macros::dec;

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
async fn heavy_test_genesis_ema_price_is_respected_for_2_intervals() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let mut config = NodeConfig::testnet();
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
        let tip_hash = chain.last().unwrap().block_hash;
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
            ctx.node_ctx.config.consensus.genesis_price, returned_ema_price,
            "Genesis price not respected for the expected duration"
        );
        assert_ne!(
            ctx.node_ctx.config.consensus.genesis_price, header.oracle_irys_price,
            "Expected the header to contain new & unique oracle irys price"
        );
        assert_ne!(
            ctx.node_ctx.config.consensus.genesis_price, header.ema_irys_price,
            "Expected the header to contain new & unique EMA irys price"
        );
    }

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_genesis_ema_price_updates_after_second_interval() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    //start node with modified config
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    // (oracle price, EMA price)
    let mut registered_prices = vec![(
        ctx.node_ctx.config.consensus.genesis_price,
        ctx.node_ctx.config.consensus.genesis_price,
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
        ctx.node_ctx.config.consensus.genesis_price, returned_ema_price,
        "After the second interval we no longer use the genesis price"
    );
    assert_eq!(
        registered_prices[2].1, returned_ema_price,
        "expected to use the EMA price registered in the 3rd block"
    );

    ctx.node_ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_oracle_price_too_high_gets_capped() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let token_price_safe_range = Amount::percentage(dec!(0.1)).unwrap();
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    config.consensus.get_mut().token_price_safe_range = token_price_safe_range;

    config.oracle = OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.0)).unwrap(),
        percent_change: Amount::percentage(dec!(0.2)).unwrap(), // every block will increase price by 20%
        // only change direction after 10 blocks
        smoothing_interval: 10,
    };

    let ctx = IrysNodeTest::new_genesis(config).start().await;

    // mine 3 blocks
    let (header_1, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (header_2, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (header_3, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();

    // assert that all of the prices are the max allowed ones (guaranteed by the mock oracle reporting inflated values)
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 4, "expected genesis + 3 new blocks");
    let genesis_block =
        get_block(ctx.node_ctx.block_tree_guard.clone(), chain[0].block_hash).unwrap();
    let mut price_prev = genesis_block.oracle_irys_price;
    for block in [header_1, header_2, header_3] {
        let max_allowed_price = price_prev.add_multiplier(token_price_safe_range).unwrap();
        assert_eq!(
            max_allowed_price, block.oracle_irys_price,
            "the registered price should be the max allowed one"
        );
        price_prev = max_allowed_price;
    }

    ctx.node_ctx.stop().await;
    Ok(())
}
