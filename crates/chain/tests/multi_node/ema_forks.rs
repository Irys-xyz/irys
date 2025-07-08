use crate::utils::IrysNodeTest;
use irys_types::{storage_pricing::Amount, NodeConfig, OracleConfig};
use rust_decimal_macros::dec;
use std::sync::Arc;

// Test verifies that EMA (Exponential Moving Average) price snapshots diverge correctly across chain forks.
// Setup:
//  - Create two nodes with different mock oracle configurations.
//  - Both nodes have a common tree.
//  - Both nodes can mine.
//
// Action: Both nodes mine blocks independently creating a fork, then node_2's longer chain is gossiped.
// Assert: EMA snapshots differ after the price adjustment interval during the fork.
// Assert: After convergence, both nodes have identical chains with matching EMA snapshots.
#[test_log::test(actix_web::test)]
async fn heavy_ema_intervals_roll_over_in_forks() -> eyre::Result<()> {
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

    let common_height = node_1.get_max_difficulty_block();
    assert_eq!(common_height, node_2.get_max_difficulty_block());
    const BLOCKS_TO_MINE_NODE_1: usize = (PRICE_ADJUSTMENT_INTERVAL as usize * 2) + 6;
    const BLOCKS_TO_MINE_NODE_2: usize = (PRICE_ADJUSTMENT_INTERVAL as usize * 2) + 8;
    // Mine blocks in parallel on both nodes to create fork
    tokio::try_join!(
        node_1.mine_blocks_without_gossip(BLOCKS_TO_MINE_NODE_1),
        node_2.mine_blocks_without_gossip(BLOCKS_TO_MINE_NODE_2)
    )?;

    let chain_node_1 = node_1.get_canonical_chain();
    let chain_node_2 = node_2.get_canonical_chain();

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
        let ema_1 = node_1.get_ema_snapshot(&block_1.block_hash).unwrap();
        let ema_2 = node_2.get_ema_snapshot(&block_2.block_hash).unwrap();

        assert_ne!(
            ema_1, ema_2,
            "ema snapshot values must differ at height {} (>= {} required for EMA differences)",
            block_1.height, min_height_for_ema_diff
        );
    }

    // Calculate expected number of blocks with different EMA values
    let last_mined_height = fork_height + BLOCKS_TO_MINE_NODE_1 as u64;
    let expected_blocks_with_ema_diff = (last_mined_height - min_height_for_ema_diff + 1) as usize;

    assert!(!blocks_with_ema_diff.is_empty());
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
    let tip_block = node_2.get_max_difficulty_block();
    assert_eq!(
        tip_block.height,
        common_height.height + BLOCKS_TO_MINE_NODE_2 as u64
    );
    node_2.gossip_block(&Arc::new(tip_block.clone()))?;

    node_1
        .wait_until_height_confirmed(tip_block.height, 200)
        .await?;

    // Verify both nodes have converged to the same canonical chain
    let final_chain_node_1 = node_1.get_canonical_chain();
    let final_chain_node_2 = node_2.get_canonical_chain();

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
        assert_eq!(block_1.height, block_2.height, "Block heights must match");

        // Get EMA snapshots for both blocks
        let ema_1 = node_1.get_ema_snapshot(&block_1.block_hash).unwrap();
        let ema_2 = node_2.get_ema_snapshot(&block_2.block_hash).unwrap();

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
