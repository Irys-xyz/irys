use crate::utils::mine_block;
use irys_actors::ema_service::EmaServiceMessage;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{
    setup_tracing_and_temp_dir, tempfile::TempDir, temporary_directory,
};
use irys_types::{irys::IrysSigner, Config};
use reth_primitives::GenesisAccount;
use rstest::rstest;

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn serial_test_genesis_ema_price_is_respected_for_2_intervals() -> eyre::Result<()> {
    // setup
    let price_adjustment_interval = 3;
    let ctx = setup(price_adjustment_interval).await?;

    // action
    // we start at 1 because the genesis block is already mined
    // let mut oracle_prices = vec![ctx.config.genesis_token_price];
    for expected_height in 1..=(price_adjustment_interval * 2) {
        let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
        dbg!(&(
            header.height,
            header.oracle_irys_price,
            header.ema_irys_price
        ));
        assert_eq!(header.height, expected_height);
        // assert_eq!(
        //     header.ema_irys_price, ctx.config.genesis_token_price,
        //     "ema price must be constant for the first interval because it does not get recalculated"
        // );
        // oracle_prices.push(header.oracle_irys_price);
        let (tx, rx) = tokio::sync::oneshot::channel();
        ctx.node
            .service_senders
            .ema
            .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
        let returnted_ema_price = rx.await?;
        assert_eq!(
            ctx.config.genesis_token_price, returnted_ema_price,
            "Genisis price not respected for the expected duration"
        );
        assert_ne!(
            ctx.config.genesis_token_price, header.oracle_irys_price,
            "Expected new & unique oracle irys price"
        );
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
    // let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
    // let (header, _payload) = mine_block(&ctx.node).await?.unwrap();
    // let (header, _payload) = mine_block(&ctx.node).await?.unwrap();

    // assert
    // dbg!(&oracle_prices);
    // let expected_price = ctx
    //     .config
    //     .genesis_token_price
    //     .calculate_ema(price_adjustment_interval, ctx.config.genesis_token_price)
    //     .unwrap();
    assert_eq!(header.height, 7);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    ctx.node
        .service_senders
        .ema
        .send(EmaServiceMessage::GetCurrentEmaForPricing { response: tx })?;
    let returnted_ema_price = rx.await?;
    assert_ne!(
        ctx.config.genesis_token_price, returnted_ema_price,
        "After the second interval we no longer use the genesis price"
    );
    // assert_ne!(
    //     header.ema_irys_price, ctx.config.genesis_token_price,
    //     "after the first interval we start calculating the EMA price"
    // );
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

// todo test each block has an adjusting oracle price
// todo test ema price gets updated after epoch block is reached
