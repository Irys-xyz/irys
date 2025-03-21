use std::time::Duration;

use crate::utils::mine_block;
use irys_actors::block_tree_service::{get_block, get_canonical_chain};
use irys_actors::ema_service::EmaServiceMessage;
use irys_actors::{
    block_tree_service::{get_block, get_canonical_chain},
    ema_service::EmaServiceMessage,
};
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::temporary_directory;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::{storage_pricing::Amount, Config, OracleConfig};
use rust_decimal_macros::dec;
use tempfile::TempDir;

#[test_log::test(tokio::test)]
async fn serial_test_genesis_ema_price_is_respected_for_2_intervals() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let ctx = setup(price_adjustment_interval).await?;

    // action
    // we start at 1 because the genesis block is already mined
    for expected_height in 1..(price_adjustment_interval * 2) {
        let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.node
            .service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
        let returnted_ema_price = rx.await?;

        // assert each new block that we mine
        assert_eq!(header.height, expected_height);
        assert_eq!(
            ctx.config.genesis_token_price, returnted_ema_price,
            "Genisis price not respected for the expected duration"
        );
        assert_ne!(
            ctx.config.genesis_token_price, header.oracle_irys_price,
            "Expected the header to contain new & unique oracle irys price"
        );
        assert_ne!(
            ctx.config.genesis_token_price, header.ema_irys_price,
            "Expected the header to contain new & unique EMA irys price"
        );
    }

    ctx.node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn serial_test_genesis_ema_price_updates_after_second_interval() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let ctx = setup(price_adjustment_interval).await?;
    // (oracle price, EMA price)
    let mut registered_prices = vec![(
        ctx.config.genesis_token_price,
        ctx.config.genesis_token_price,
    )];
    // mine 6 blocks
    for _expected_height in 1..(price_adjustment_interval * 2) {
        let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
        registered_prices.push((header.oracle_irys_price, header.ema_irys_price));
    }

    // action -- mine a new block. This pushes the system to use a new EMA rather than the genesis EMA
    let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    ctx.node
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
        ctx.config.genesis_token_price, returnted_ema_price,
        "After the second interval we no longer use the genesis price"
    );
    assert_eq!(
        registered_prices[2].1, returnted_ema_price,
        "expected to use the EMA price registered in the 3rd block"
    );

    ctx.node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn serial_test_oracle_price_too_high_gets_capped() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let token_price_safe_range = Amount::percentage(dec!(0.1)).unwrap();
    let ctx = setup_with_config(Config {
        price_adjustment_interval,
        oracle_config: OracleConfig::Mock {
            initial_price: Amount::token(dec!(1.0)).unwrap(),
            percent_change: Amount::percentage(dec!(0.2)).unwrap(), // every block will increase price by 20%
            // only change direction after 10 blocks
            smoothing_interval: 10,
        },
        token_price_safe_range: token_price_safe_range.clone(), // 10% allowed diff from the previous oracle
        ..Config::testnet()
    })
    .await?;

    // mine 3 blocks
    let (header_1, _payload) = mine_block(&ctx.node).await?.unwrap();
    let (header_2, _payload) = mine_block(&ctx.node).await?.unwrap();
    let (header_3, _payload) = mine_block(&ctx.node).await?.unwrap();

    // assert that all of the prices are the max allowed ones (guaranteed by the mock oracle reporting inflated values)
    let (chain, ..) = get_canonical_chain(ctx.node.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 4, "expected genesis + 3 new blocks");
    let genesis_block = get_block(ctx.node.block_tree_guard.clone(), chain[0].0)
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

    ctx.node.stop().await;
    Ok(())
}

struct TestCtx {
    config: Config,
    node: IrysNodeCtx,
    #[expect(
        dead_code,
        reason = "to prevent drop() being called and cleaning up resources"
    )]
    temp_dir: TempDir,
}

async fn setup(price_adjustment_interval: u64) -> eyre::Result<TestCtx> {
    let testnet_config = Config {
        price_adjustment_interval,
        ..Config::testnet()
    };
    setup_with_config(testnet_config).await
}

async fn setup_with_config(mut config: Config) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some("test_ema"), false);
    config.base_directory = temp_dir.path().to_path_buf();
    let node = IrysNode::new(config.clone(), true).init().await?;
    Ok(TestCtx {
        config,
        node,
        temp_dir,
    })
}
