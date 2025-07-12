use crate::utils::IrysNodeTest;
use irys_domain::get_canonical_chain;
use irys_testing_utils::utils::temporary_directory;
use irys_types::NodeConfig;

#[test_log::test(actix_web::test)]
async fn heavy_test_can_resume_from_genesis_startup_with_ctx() -> eyre::Result<()> {
    // setup
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new_genesis(config.clone());

    // retrieve block_migration_depth for use later
    let mut consensus = node.cfg.consensus.clone();
    let block_migration_depth = consensus.get_mut().block_migration_depth;

    // action:
    // 1. start the genesis node;
    // 2. mine 2 new blocks
    // 3. This rolls over the epoch (meaning the genesis + second block get finalized and written to disk)
    let ctx = node.start().await;
    ctx.mine_block().await?;
    let header_1 = ctx.get_block_by_height(1).await?;
    ctx.mine_blocks(block_migration_depth.try_into()?).await?;
    ctx.wait_until_block_index_height(1, 5).await?;

    // restart the node
    let ctx = ctx.stop().await.start().await;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().height,
        "expect only the first manually mined block to be saved"
    );
    assert_eq!(
        chain.len(),
        2,
        "we expect the genesis block + 1 new block (the second block does not get saved)"
    );
    ctx.mine_block().await?;
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 3, "we expect the genesis block + 2 new blocks");

    ctx.stop().await;
    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_test_can_resume_from_genesis_startup_no_ctx() -> eyre::Result<()> {
    // setup consistent test directory for this test
    let temp_dir = temporary_directory(None, false);
    let test_dir = temp_dir.path().to_path_buf();

    let config = NodeConfig::testnet();
    let mut node = IrysNodeTest::new_genesis(config.clone());
    node.cfg.base_directory = test_dir.clone();

    // retrieve block_migration_depth for use later
    let mut consensus = node.cfg.consensus.clone();
    let block_migration_depth = consensus.get_mut().block_migration_depth;

    // action:
    // 1. start the genesis node;
    // 2. mine 2 new blocks
    // 3. This rolls over the epoch (meaning the genesis + second block get finalized and written to disk)
    let ctx = node.start().await;
    ctx.mine_block().await?;
    let header_1 = ctx.get_block_by_height(1).await?;
    ctx.mine_blocks(block_migration_depth.try_into()?).await?;
    ctx.wait_until_block_index_height(1, 5).await?;
    // stop the node
    ctx.stop().await;

    // start a new instance to take over from the old one that has been completely stopped
    // i.e. it's important this test runs stop() run start() on a new instance with no context transferred via self.
    let mut node = IrysNodeTest::new_genesis(config.clone());
    node.cfg.base_directory = test_dir;
    let ctx = node.start().await;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().height,
        "expect only the first manually mined block to be saved"
    );
    assert_eq!(
        chain.len(),
        2,
        "we expect the genesis block + 1 new block (the second block does not get saved)"
    );
    ctx.mine_block().await?;
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 3, "we expect the genesis block + 2 new blocks");

    ctx.stop().await;
    Ok(())
}

// #[test_log::test(tokio::test)]
// #[should_panic(expected = "IrysNodeCtx must be stopped before all instances are dropped")]
// async fn heavy_test_stop_guard() -> () {
//     let node = IrysNodeTest::default_async().start().await;
//     drop(node);
// }
