use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use irys_actors::{
    async_trait, reth_ethereum_primitives, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::{
    storage_pricing::Amount, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader,
    NodeConfig, OracleConfig,
};
use reth::payload::EthBuiltPayload;
use rust_decimal_macros::dec;

#[test_log::test(actix_web::test)]
async fn heavy_ema_states_valid_across_forks() -> eyre::Result<()> {
    // setup
    const PRICE_ADJUSTMENT_INTERVAL: u64 = 2;
    let num_blocks_in_epoch = 13;
    let seconds_to_wait = 20;
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    genesis_config
        .consensus
        .get_mut()
        .ema
        .price_adjustment_interval = PRICE_ADJUSTMENT_INTERVAL;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let node_1 = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    node_1.start_public_api().await;
    let mut peer_config = node_1.testnet_peer_with_signer(&peer_signer);
    peer_config.oracle = OracleConfig::Mock {
        initial_price: Amount::token(dec!(1.01)).unwrap(),
        incremental_change: Amount::token(dec!(0.005)).unwrap(),
        smoothing_interval: 3,
    };

    let node_2 = node_1
        .testnet_peer_with_assignments_and_name(peer_config, "PEER")
        .await;

    let common_height = node_1.get_max_difficulty_block().await;
    assert_eq!(common_height, node_2.get_max_difficulty_block().await);
    const BLOCKS_TO_MINE_NODE_1: usize = (PRICE_ADJUSTMENT_INTERVAL as usize * 2) + 3;
    const BLOCKS_TO_MINE_NODE_2: usize = (PRICE_ADJUSTMENT_INTERVAL as usize * 2) + 5;
    node_1
        .mine_blocks_without_gossip(BLOCKS_TO_MINE_NODE_1)
        .await?;
    node_2
        .mine_blocks_without_gossip(BLOCKS_TO_MINE_NODE_2)
        .await?;

    let chain_node_1 = node_1
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;
    let chain_node_2 = node_2
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;

    // Find the height where chains diverged (last common block)
    let fork_height = common_height.height;

    // Ensure fork happens after the special EMA handling period
    assert!(
        fork_height >= (PRICE_ADJUSTMENT_INTERVAL * 2),
        "Fork height {} must be >= {} (2 price adjustment intervals) for this test",
        fork_height,
        PRICE_ADJUSTMENT_INTERVAL * 2
    );

    // Calculate minimum height where EMA differences should appear
    // Standard case: need at least 2 intervals for oracle differences to affect EMA
    let min_height_for_ema_diff = fork_height + (PRICE_ADJUSTMENT_INTERVAL * 2);

    // Compare blocks that have diverged
    let diverged_blocks: Vec<_> = chain_node_1
        .iter()
        .zip(chain_node_2.iter())
        .filter(|(b1, b2)| b1.block_hash != b2.block_hash)
        .collect();

    let blocks_compared = diverged_blocks.len();
    assert_eq!(
        blocks_compared, BLOCKS_TO_MINE_NODE_1,
        "expect to have compared the len of the shortest fork amount of blocks"
    );

    // Process blocks where EMA should differ
    let blocks_with_ema_diff: Vec<_> = diverged_blocks
        .iter()
        .filter(|(b1, _)| b1.height >= min_height_for_ema_diff)
        .collect();

    tracing::info!(
        "Fork at height {}, checking EMA differences starting from height {} ({} blocks)",
        fork_height,
        min_height_for_ema_diff,
        blocks_with_ema_diff.len()
    );

    // Verify all diverged blocks have different hashes
    for (block_1, block_2) in &diverged_blocks {
        assert_eq!(block_1.height, block_2.height, "heights must be the same");
        assert_ne!(
            block_1.block_hash, block_2.block_hash,
            "block hashes must differ"
        );
    }

    // Verify EMA differences for blocks after the delay period
    for (block_1, block_2) in &blocks_with_ema_diff {
        let ema_1 = node_1
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_1.block_hash)
            .unwrap();
        let ema_2 = node_2
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_2.block_hash)
            .unwrap();

        assert_ne!(
            ema_1, ema_2,
            "ema snapshot values must differ at height {} (>= {} required for EMA differences)",
            block_1.height, min_height_for_ema_diff
        );
    }

    // Calculate expected number of blocks with different EMA values
    // We mined BLOCKS_TO_MINE_NODE_1 blocks after fork_height
    // EMA differences start at min_height_for_ema_diff
    // So we expect: (fork_height + BLOCKS_TO_MINE_NODE_1) - min_height_for_ema_diff + 1
    let last_mined_height = fork_height + BLOCKS_TO_MINE_NODE_1 as u64;
    let expected_blocks_with_ema_diff = (last_mined_height - min_height_for_ema_diff + 1) as usize;

    assert!(blocks_with_ema_diff.len() > 0);
    assert_eq!(
        blocks_with_ema_diff.len(),
        expected_blocks_with_ema_diff,
        "Expected exactly {} blocks with different EMA values. \
         Fork at height {}, mined {} blocks (up to height {}), \
         EMA differences start at height {}",
        expected_blocks_with_ema_diff,
        fork_height,
        BLOCKS_TO_MINE_NODE_1,
        last_mined_height,
        min_height_for_ema_diff
    );

    node_2.gossip_enable();
    node_1.gossip_enable();

    // converge to the longest chain
    let tip_block = node_2.get_max_difficulty_block().await;
    assert_eq!(
        tip_block.height,
        common_height.height + BLOCKS_TO_MINE_NODE_2 as u64
    );
    node_2.gossip_block(&tip_block)?;

    node_1
        .wait_until_height_confirmed(tip_block.height, 200)
        .await?;

    // Verify both nodes have converged to the same canonical chain
    let final_chain_node_1 = node_1
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;
    let final_chain_node_2 = node_2
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain()
        .0;

    // Both chains should have the same length
    assert_eq!(
        final_chain_node_1.len(),
        final_chain_node_2.len(),
        "Both nodes should have chains of equal length after convergence"
    );

    // Verify all blocks in the canonical chain are identical and have identical EMA snapshots
    for (block_1, block_2) in final_chain_node_1.iter().zip(final_chain_node_2.iter()) {
        assert_eq!(
            block_1.block_hash, block_2.block_hash,
            "Block hashes must be identical at height {} after convergence",
            block_1.height
        );
        assert_eq!(
            block_1.height, block_2.height,
            "Block heights must match"
        );

        // Get EMA snapshots for both blocks
        let ema_1 = node_1
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_1.block_hash)
            .unwrap();
        let ema_2 = node_2
            .node_ctx
            .block_tree_guard
            .read()
            .get_ema_snapshot(&block_2.block_hash)
            .unwrap();

        assert_eq!(
            ema_1, ema_2,
            "EMA snapshots must be identical for block at height {} after convergence",
            block_1.height
        );
    }

    tracing::info!(
        "Successfully verified convergence: {} blocks with identical EMA snapshots",
        final_chain_node_1.len()
    );

    node_2.stop().await;
    node_1.stop().await;

    Ok(())
}
