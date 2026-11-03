use crate::utils::IrysNodeTest;
use irys_domain::get_canonical_chain;
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::NodeConfig;

#[test_log::test(tokio::test)]
async fn heavy_test_can_resume_from_genesis_startup_with_ctx() -> eyre::Result<()> {
    // setup
    let config = NodeConfig::testing();
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
    ctx.wait_until_block_index_height(1, 15).await?;

    // restart the node
    let ctx = ctx.stop().await.start().await;

    // assert -- expect that the non genesis node can continue with the genesis data
    let (chain, ..) = get_canonical_chain(ctx.node_ctx.block_tree_guard.clone())
        .await
        .unwrap();
    assert_eq!(
        header_1.height,
        chain.last().unwrap().height(),
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

#[test_log::test(tokio::test)]
async fn heavy_test_can_resume_from_genesis_startup_no_ctx() -> eyre::Result<()> {
    // setup consistent test directory for this test
    let temp_dir = TempDirBuilder::new().build();
    let test_dir = temp_dir.path().to_path_buf();

    let config = NodeConfig::testing();
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
    ctx.wait_until_block_index_height(1, 15).await?;
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
        chain.last().unwrap().height(),
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

/// Reth makes a block canonical in memory when it applies a fork-choice update, and writes it to
/// disk later, on its own persistence task. A crash while Reth's disk is behind the block index
/// tip leaves Reth without blocks the block index names, and payload repair cannot fill that gap
/// at startup: it runs after the startup fork-choice update.
///
/// This test builds that disk state without a crash: it stops the node cleanly, then unwinds
/// Reth's database to the block below the index tip.
///
/// Invariant: startup stops with an error that names the block index tip, Reth's disk tip and
/// the `irys-cli rollback-blocks` command that fixes it.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn heavy_restart_names_rollback_when_reth_is_behind_the_block_index_tip() -> eyre::Result<()>
{
    use futures::FutureExt as _;

    const SECONDS_TO_WAIT: usize = 20;
    // Reth's disk tip after the simulated crash, in blocks below the Irys index tip.
    const RETH_BLOCKS_BEHIND_INDEX_TIP: u64 = 1;

    let config = NodeConfig::testing();
    let migration_depth = u64::from(config.consensus_config().block_migration_depth);
    let node = IrysNodeTest::new_genesis(config).start().await;

    let target_index_height = RETH_BLOCKS_BEHIND_INDEX_TIP + 1;
    node.mine_blocks((target_index_height + migration_depth).try_into()?)
        .await?;
    node.wait_until_block_index_height(target_index_height, SECONDS_TO_WAIT)
        .await?;
    let index_tip_height = node.node_ctx.block_index_guard.read().latest_height();
    let reth_disk_tip = node
        .get_block_by_height(index_tip_height - RETH_BLOCKS_BEHIND_INDEX_TIP)
        .await?;
    let chainspec = node.node_ctx.reth_handle.chain_spec();

    let stopped = node.stop().await;

    let unwind_runtime =
        reth::tasks::RuntimeBuilder::new(reth::tasks::RuntimeConfig::default().with_tokio(
            reth::tasks::TokioConfig::existing_handle(tokio::runtime::Handle::current()),
        ))
        .build()?;
    irys_reth_node_bridge::unwind::unwind_to(
        &stopped.cfg,
        chainspec,
        reth_disk_tip.evm_block_hash,
        unwind_runtime,
    )
    .await?;

    let restart = std::panic::AssertUnwindSafe(stopped.start())
        .catch_unwind()
        .await;
    let Err(panic) = restart else {
        panic!("startup must stop when Reth's disk tip is below the block index tip");
    };
    let startup_error = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    let expected_parts = [
        format!(
            "Reth's database does not hold the Irys block index tip (height {index_tip_height}"
        ),
        format!("Reth's highest block on disk is {}", reth_disk_tip.height),
        format!("irys-cli rollback-blocks to-block {}", reth_disk_tip.height),
    ];
    for expected in &expected_parts {
        assert!(
            startup_error.contains(expected.as_str()),
            "the startup error must name the block index tip, Reth's disk tip and the rollback \
             command; missing {expected:?} in: {startup_error}"
        );
    }
    Ok(())
}

/// Invariant: the block index tip is always a block Reth has on disk, so an unclean stop never
/// leaves the block index ahead of Reth. The block tree migrates a block only after RethService
/// acks the fork-choice update, and RethService acks only when the confirmed block is on disk.
#[test_log::test(tokio::test)]
async fn heavy_block_index_tip_is_always_on_reth_disk() -> eyre::Result<()> {
    use reth::providers::{BlockNumReader as _, DatabaseProviderFactory as _};

    // Enough blocks for the block index to advance several times past genesis.
    const BLOCKS_TO_MINE: u64 = 10;

    let node = IrysNodeTest::new_genesis(NodeConfig::testing())
        .start()
        .await;
    for _ in 0..BLOCKS_TO_MINE {
        node.mine_block().await?;
        let index_tip_height = node.node_ctx.block_index_guard.read().latest_height();
        let index_tip_header = node.get_block_by_height_from_index(index_tip_height, false)?;
        let on_disk = node
            .node_ctx
            .reth_node_adapter
            .reth_node
            .provider
            .database_provider_ro()?
            .block_number(index_tip_header.evm_block_hash)?
            .is_some();
        assert!(
            on_disk,
            "the block index tip (height {}) must be on Reth's disk",
            index_tip_header.height
        );
    }

    node.stop().await;
    Ok(())
}
