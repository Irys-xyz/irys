use crate::utils::{mine_block, IrysNodeTest};
use irys_actors::block_tree_service::get_canonical_chain;
use irys_types::NodeConfig;
use std::time::Duration;

#[test_log::test(actix_web::test)]
async fn heavy_test_can_resume_from_genesis_startup_with_ctx() -> eyre::Result<()> {
    // setup
    let node = IrysNodeTest::default_async().await;

    // action:
    // 1. start the genesis node;
    // 2. mine 2 new blocks
    // 3. This rolls over the epoch (meaning the genesis + second block get finalized and written to disk)
    let ctx = node.start().await;
    let (header_1, ..) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (_header_2, ..) = mine_block(&ctx.node_ctx).await?.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    // restart the node
    let ctx = ctx.stop().await.start().await;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().1,
        "expect only the first manually mined block to be saved"
    );
    assert_eq!(
        chain.len(),
        2,
        "we expect the genesis block + 1 new block (the second block does not get saved)"
    );
    mine_block(&ctx.node_ctx).await?;
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 3, "we expect the genesis block + 2 new blocks");

    ctx.stop().await;
    Ok(())
}

#[test_log::test(actix_web::test)]
async fn heavy_test_can_resume_from_genesis_startup_no_ctx() -> eyre::Result<()> {
    // setup
    let config = NodeConfig::testnet();
    let node = IrysNodeTest::new(config.clone()).await;

    // action:
    // 1. start the genesis node;
    // 2. mine 2 new blocks
    // 3. This rolls over the epoch (meaning the genesis + second block get finalized and written to disk)
    let ctx = node.start().await;
    let (header_1, ..) = mine_block(&ctx.node_ctx).await?.unwrap();
    let (_header_2, ..) = mine_block(&ctx.node_ctx).await?.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    // stop the node
    ctx.stop().await;

    let node = IrysNodeTest::new(config).await;
    let ctx = node.start().await;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().1,
        "expect only the first manually mined block to be saved"
    );
    assert_eq!(
        chain.len(),
        2,
        "we expect the genesis block + 1 new block (the second block does not get saved)"
    );
    mine_block(&ctx.node_ctx).await?;
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(chain.len(), 3, "we expect the genesis block + 2 new blocks");

    ctx.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
#[should_panic(expected = "IrysNodeCtx must be stopped before all instances are dropped")]
async fn heavy_test_stop_guard() -> () {
    let node = IrysNodeTest::default_async().await.start().await;
    drop(node);
}
