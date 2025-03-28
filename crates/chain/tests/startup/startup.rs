use crate::utils::mine_block;
use irys_actors::block_tree_service::get_canonical_chain;
use irys_chain::{start_irys_node, IrysNode};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
use irys_types::Config;
use std::time::Duration;

#[test_log::test(tokio::test)]
async fn heavy_test_can_resume_from_genesis_startup() -> eyre::Result<()> {
    // setup
    let temp_dir = temporary_directory(Some("test_startup"), false);
    let config = Config {
        num_blocks_in_epoch: 2, // every 2 blocks we finalize a new block
        base_directory: temp_dir.path().to_path_buf(),
        ..Config::testnet()
    };
    let mut genesis_node = IrysNode::new(config.clone(), true);
    let mut non_genesis_node = IrysNode::new(config.clone(), false);

    // action:
    // 1. start the genesis node;
    // 2. mine 2 new blocks
    // 3. This rolls over the epoch (meaning the genesis + second block get finalized and written to disk)
    let ctx = genesis_node.init().await?;
    let (header_1, ..) = mine_block(&ctx).await?.unwrap();
    let (_header_2, ..) = mine_block(&ctx).await?.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    ctx.stop().await;
    let ctx = non_genesis_node.init().await?;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().1,
        "expect only the first manually miend block to be saved"
    );
    assert_eq!(
        chain.len(),
        2,
        "we expect the genesis block + 1 new block (the second block does not get saved)"
    );
    mine_block(&ctx).await?;
    let (chain, ..) = get_canonical_chain(ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 3, "we expect the genesis block + 2 new blocks");

    ctx.stop().await;
    Ok(())
}

#[tokio::test]
#[should_panic(expected = "IrysNodeCtx must be stopped before all instances are dropped")]
async fn heavy_test_stop_guard() -> () {
    let temp_dir = setup_tracing_and_temp_dir(Some("heavy_test_stop_guard"), false);

    let testnet_config = Config::testnet();
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();

    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let _node = start_irys_node(config, storage_config, testnet_config.clone())
        .await
        .unwrap();
    ()
}
