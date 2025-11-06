use irys_types::NodeConfig;
use rust_decimal_macros::dec;

use crate::{utils::IrysNodeTest, validation::send_block_to_block_tree};

/// Ensures that the node adjusts its mining difficulty after the configured
/// number of blocks and that the `last_diff_timestamp` metadata is updated to
/// the timestamp of the block that triggered the adjustment.
#[test_log::test(tokio::test)]
async fn difficulty_adjusts_and_timestamp_updates() -> eyre::Result<()> {
    // max time to wait for block validations
    let max_seconds = 10;

    // Spin up a test node and tweak the consensus configuration so that the
    // difficulty recalculates after every second block.
    let mut node = IrysNodeTest::default_async();
    {
        // Modify the difficulty adjustment settings:
        // - `difficulty_adjustment_interval = 2` means the difficulty should
        //   be recalculated after two blocks.
        // - `min_difficulty_adjustment_factor = 0` removes the lower bound so
        //   the difficulty is guaranteed to change when the interval is hit.
        let consensus = node.cfg.consensus.get_mut();
        consensus
            .difficulty_adjustment
            .difficulty_adjustment_interval = 2;
        consensus
            .difficulty_adjustment
            .min_difficulty_adjustment_factor = dec!(0);
    }

    // Start the node so we can mine blocks against it.
    let node = node.start().await;

    // Mine the first block. The difficulty adjustment interval has not yet been
    // reached, so `last_diff_timestamp` should remain the previous value and
    // therefore not match the block's timestamp.
    let block1 = node.mine_block().await?;
    node.wait_until_height(1, max_seconds).await?;
    assert_ne!(block1.last_diff_timestamp, block1.timestamp);

    // Mine a second block which hits the adjustment interval. At this point the
    // difficulty should change and `last_diff_timestamp` should be updated to
    // the new block's timestamp.
    let block2 = node.mine_block().await?;
    node.wait_until_height(2, max_seconds).await?;
    assert_ne!(block2.diff, block1.diff);
    assert_eq!(block2.last_diff_timestamp, block2.timestamp);
    assert_ne!(block2.last_diff_timestamp, block1.last_diff_timestamp);

    // Shut down the node to clean up the test environment.
    node.stop().await;
    Ok(())
}

/// Ensures that the node adjusts its mining difficulty after the configured
/// number of blocks and that the `last_diff_timestamp` metadata is updated to
/// the timestamp of the block that triggered the adjustment.
#[test_log::test(tokio::test)]
async fn heavy_tip_updated_correctly() -> eyre::Result<()> {
    // max time to wait for block validations
    let max_seconds = 10;
    let num_blocks_in_epoch = 2;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 6;
    genesis_config.consensus.get_mut().block_tree_depth = 20;
    // genesis_config
    //     .consensus
    //     .get_mut()
    //     .difficulty_adjustment
    //     .difficulty_adjustment_interval = 2;
    // genesis_config
    //     .consensus
    //     .get_mut()
    //     .difficulty_adjustment
    //     .min_difficulty_adjustment_factor = dec!(0);

    let test_signer = genesis_config.new_random_signer();
    let test_signer_2 = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer, &test_signer_2]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;
    let fork_creator_1 = genesis_node
        .testing_peer_with_assignments(&test_signer)
        .await?;
    let fork_creator_2 = genesis_node
        .testing_peer_with_assignments(&test_signer_2)
        .await?;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order

    // temp disable block valiation
    genesis_node.node_ctx.set_validation_enabled(false);

    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...fc1");
    tracing::error!("...");
    fork_creator_2.gossip_disable();
    fork_creator_1.gossip_disable();
    genesis_node.gossip_disable();
    let fc_block_1 = fork_creator_1.mine_block_without_gossip().await?;
    let fc_block_2 = fork_creator_1.mine_block_without_gossip().await?;
    let fc_block_3 = fork_creator_1.mine_block_without_gossip().await?;
    let fc_block_4 = fork_creator_1.mine_block_without_gossip().await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...fc2");
    tracing::error!("...");

    let block_1 = fork_creator_2.mine_block_without_gossip().await?;
    let block_2 = fork_creator_2.mine_block_without_gossip().await?;
    let block_3 = fork_creator_2.mine_block_without_gossip().await?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order

    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...broadcasting");
    // for (block, eth_block) in [&block_1, &fc_block_1, &block_2, &fc_block_2, &block_3, &fc_block_3, &fc_block_4].iter() {
    let order = [
        &block_1,
        &block_2,
        &fc_block_1,
        &fc_block_2,
        &block_3,
        &fc_block_3,
        &fc_block_4,
    ];
    for (block, eth_block) in order.iter() {
        tracing::error!(block_heght = block.height,  ?block.cumulative_diff, "block");
        send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;
        // genesis_node.node_ctx.block_pool.execution_payload_provider.add_payload_to_cache(eth_block.block().clone()).await;
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...validating");
    genesis_node.node_ctx.set_validation_enabled(true);
    for (block, eth_block) in order.iter() {
        tracing::error!(block_heght = block.height,  ?block.cumulative_diff, "block");
        // send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;
        genesis_node
            .node_ctx
            .block_pool
            .execution_payload_provider
            .add_payload_to_cache(eth_block.block().clone())
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await; // broadcast blocks in the proper order
    }

    tokio::time::sleep(std::time::Duration::from_secs(10)).await; // broadcast blocks in the proper order

    // Shut down the node to clean up the test environment.
    fork_creator_2.stop().await;
    genesis_node.stop().await;
    Ok(())
}

/// Ensures that the node adjusts its mining difficulty after the configured
/// number of blocks and that the `last_diff_timestamp` metadata is updated to
/// the timestamp of the block that triggered the adjustment.
#[test_log::test(tokio::test)]
async fn heavy_tip_updated_correctly_part_two() -> eyre::Result<()> {
    // max time to wait for block validations
    let max_seconds = 10;
    let num_blocks_in_epoch = 2;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 2;
    genesis_config.consensus.get_mut().block_tree_depth = 20;
    // genesis_config
    //     .consensus
    //     .get_mut()
    //     .difficulty_adjustment
    //     .difficulty_adjustment_interval = 2;
    // genesis_config
    //     .consensus
    //     .get_mut()
    //     .difficulty_adjustment
    //     .min_difficulty_adjustment_factor = dec!(0);

    let test_signer = genesis_config.new_random_signer();
    let test_signer_2 = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&test_signer, &test_signer_2]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;
    let fork_creator_1 = genesis_node
        .testing_peer_with_assignments(&test_signer)
        .await?;
    let fork_creator_2 = genesis_node
        .testing_peer_with_assignments(&test_signer_2)
        .await?;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order

    // temp disable block valiation
    genesis_node.node_ctx.set_validation_enabled(false);

    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...fc1");
    tracing::error!("...");
    fork_creator_2.gossip_disable();
    fork_creator_1.gossip_disable();
    genesis_node.gossip_disable();

    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...broadcasting");
    // for (block, eth_block) in [&block_1, &fc_block_1, &block_2, &fc_block_2, &block_3, &fc_block_3, &fc_block_4].iter() {
    let order = [
        &fork_creator_1.mine_block_without_gossip().await?,
        &fork_creator_1.mine_block_without_gossip().await?,
        &fork_creator_2.mine_block_without_gossip().await?,
        &fork_creator_2.mine_block_without_gossip().await?,
        &fork_creator_2.mine_block_without_gossip().await?,
        &fork_creator_1.mine_block_without_gossip().await?,
        &fork_creator_1.mine_block_without_gossip().await?,
    ];
    for (block, eth_block) in order.iter() {
        tracing::error!(block_heght = block.height,  ?block.cumulative_diff, "block");
        send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;
        // genesis_node.node_ctx.block_pool.execution_payload_provider.add_payload_to_cache(eth_block.block().clone()).await;
    }

    tokio::time::sleep(std::time::Duration::from_secs(1)).await; // broadcast blocks in the proper order
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...");
    tracing::error!("...validating");
    genesis_node.node_ctx.set_validation_enabled(true);
    for (block, eth_block) in order.iter() {
        tracing::error!(block_heght = block.height,  ?block.cumulative_diff, "block");
        // send_block_to_block_tree(&genesis_node.node_ctx, block.clone(), vec![], false).await?;
        genesis_node
            .node_ctx
            .block_pool
            .execution_payload_provider
            .add_payload_to_cache(eth_block.block().clone())
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await; // broadcast blocks in the proper order
    }

    tokio::time::sleep(std::time::Duration::from_secs(10)).await; // broadcast blocks in the proper order

    // Shut down the node to clean up the test environment.
    fork_creator_2.stop().await;
    genesis_node.stop().await;
    Ok(())
}
