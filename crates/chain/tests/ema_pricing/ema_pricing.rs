use std::time::Duration;

use crate::utils::{mine_block, IrysNodeTest};
use irys_actors::{
    block_tree_service::{get_block, get_canonical_chain},
    ema_service::EmaServiceMessage,
};
use irys_types::{storage_pricing::Amount, Config, NodeConfig, OracleConfig};
use rust_decimal_macros::dec;

#[test_log::test(tokio::test)]
async fn heavy_test_genesis_ema_price_is_respected_for_2_intervals() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().ema.price_adjustment_interval = price_adjustment_interval;
    let ctx = IrysNodeTest::new_genesis(config).await.start().await;

    // action
    // we start at 1 because the genesis block is already mined
    for expected_height in 1..(price_adjustment_interval * 2) {
        let (header, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.node_ctx
            .service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
        let returnted_ema_price = rx.await?;

        // assert each new block that we mine
        assert_eq!(header.height, expected_height);
        assert_eq!(
            ctx.node_ctx.config.consensus.genesis_price, returnted_ema_price,
            "Genisis price not respected for the expected duration"
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
    let ctx = IrysNodeTest::new_genesis(config).await.start().await;
    // (oracle price, EMA price)
    let mut registered_prices = vec![(
        ctx.node_ctx.config.consensus.genesis_price,
        ctx.node_ctx.config.consensus.genesis_price,
    )];
    // mine 6 blocks
    for _expected_height in 1..(price_adjustment_interval * 2) {
        let (header, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
        registered_prices.push((header.oracle_irys_price, header.ema_irys_price));
    }

    // action -- mine a new block. This pushes the system to use a new EMA rather than the genesis EMA
    let (header, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    ctx.node_ctx
        .service_senders
        .ema
        .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
    let returnted_ema_price = rx.await?;

    // assert
    assert_eq!(
        header.height, 6,
        "expected the 7th block to be mined (height = 6)"
    );
    assert_ne!(
        ctx.node_ctx.config.consensus.genesis_price, returnted_ema_price,
        "After the second interval we no longer use the genesis price"
    );
    assert_eq!(
        registered_prices[2].1, returnted_ema_price,
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

    let ctx = IrysNodeTest::new_genesis(config).await.start().await;

    // mine 3 blocks
    let (header_1, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (header_2, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (header_3, _payload) = mine_block(&ctx.node_ctx).await?.unwrap();

    // assert that all of the prices are the max allowed ones (guaranteed by the mock oracle reporting inflated values)
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 4, "expected genesis + 3 new blocks");
    let genesis_block = get_block(ctx.node_ctx.block_tree_guard.clone(), chain[0].0)
        .await
        .unwrap()
        .unwrap();
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
