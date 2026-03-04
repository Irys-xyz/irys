use irys_actors::block_tree_service::ValidationResult;
use std::sync::Arc;

use irys_types::{BlockBody, NodeConfig, SealedBlock};
use rust_decimal_macros::dec;

use crate::{
    utils::{wait_for_block_event, IrysNodeTest},
    validation::send_block_to_block_tree,
};

/// Ensures that the node adjusts its mining difficulty after the configured
/// number of blocks and that the `last_diff_timestamp` metadata is updated to
/// the timestamp of the block that triggered the adjustment.
#[test_log::test(tokio::test)]
async fn heavy3_difficulty_adjusts_and_timestamp_updates() -> eyre::Result<()> {
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

/// Create 3 nodes:
/// - genesis that does not mine after peers come online
/// - 2 peers that create competing forks
///
/// Scenario: all blocks get gossiped to the `genesis` node together, but we
/// control the order of which they get validated by selectively broadacting the execution payloads.
///
/// Expectation:
/// - we only mark the tip for the blocks that are actually the newest validated "highest cumulative diff" block.
/// (regression protection: `mark_tip` used to be called on every single validated block, even if it had a lesser cumulative diff)
#[test_log::test(tokio::test)]
async fn slow_heavy4_tip_updated_correctly_in_forks_with_variying_cumulative_difficulties(
) -> eyre::Result<()> {
    // max time to wait for block validations
    let max_seconds = 10;
    let num_blocks_in_epoch = 2;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = 2;
    genesis_config.consensus.get_mut().block_tree_depth = 20;
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

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // temp disable block validation
    genesis_node.node_ctx.set_validation_enabled(false);

    fork_creator_2.gossip_disable();
    fork_creator_1.gossip_disable();
    genesis_node.gossip_disable();

    tracing::error!("...broadcasting");
    let order = [
        (&fork_creator_1.mine_block_without_gossip().await?, true),
        (&fork_creator_1.mine_block_without_gossip().await?, true),
        (&fork_creator_2.mine_block_without_gossip().await?, false),
        (&fork_creator_2.mine_block_without_gossip().await?, false),
        (&fork_creator_2.mine_block_without_gossip().await?, true),
        (&fork_creator_1.mine_block_without_gossip().await?, false),
        (&fork_creator_1.mine_block_without_gossip().await?, true),
    ];
    for ((block, _eth_block, transactions), _new_tip) in order.iter() {
        tracing::error!(block_heght = block.height,  ?block.cumulative_diff, "block");
        let body = BlockBody {
            block_hash: block.block_hash,
            commitment_transactions: transactions.all_system_txs().cloned().collect(),
            data_transactions: transactions.all_data_txs().cloned().collect(),
        };
        let sealed_block = Arc::new(SealedBlock::new(Arc::clone(block), body)?);

        send_block_to_block_tree(&genesis_node.node_ctx, sealed_block, false).await?;
    }

    // Subscribe BEFORE enabling validation to avoid missing events that fire
    // between re-enable and subscribe.
    let mut block_state_rx = genesis_node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();

    tracing::error!("...validating");
    genesis_node.node_ctx.set_validation_enabled(true);

    // Feed execution payloads one at a time to control the validation order.
    // After each block validates, assert whether it became the new tip.
    for ((block, eth_block, _), new_tip) in order.iter() {
        tracing::error!(block_height = block.height, ?block.cumulative_diff, "feeding payload");
        genesis_node
            .node_ctx
            .block_pool
            .execution_payload_provider
            .add_payload_to_cache(eth_block.block().clone())
            .await;

        // 60s timeout (not max_seconds) because CI validation can be slower than mining
        wait_for_block_event(&mut block_state_rx, 60, |ev| {
            ev.block_hash == block.block_hash
                && matches!(ev.validation_result, ValidationResult::Valid)
        })
        .await?;

        if *new_tip {
            assert_eq!(
                genesis_node.node_ctx.block_tree_guard.read().tip,
                block.block_hash,
                "block at height {} should be the new tip",
                block.height,
            );
        } else {
            assert_ne!(
                genesis_node.node_ctx.block_tree_guard.read().tip,
                block.block_hash,
                "block at height {} should NOT be the tip",
                block.height,
            );
        }
    }

    // Shut down the node to clean up the test environment.
    fork_creator_2.stop().await;
    fork_creator_1.stop().await;
    genesis_node.stop().await;
    Ok(())
}
