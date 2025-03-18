use crate::utils::mine_block;
use irys_actors::ema_service::EmaServiceMessage;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::temporary_directory;
use irys_types::Config;
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
    let temp_dir = temporary_directory(Some("test_ema"), false);
    let testnet_config = Config {
        price_adjustment_interval,
        ..Config::testnet()
    };
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    Ok(TestCtx {
        config: testnet_config,
        node,
        temp_dir,
    })
}
