//! Comprehensive tests for block producer rebuild logic and solution validation.
//!
//! This module tests various scenarios where the block producer must decide whether to:
//! - Continue building with the current solution
//! - Rebuild the block with a new parent but same solution
//! - Discard the solution entirely due to invalidity
//!
//! Test scenarios covered:
//! 1. VDF too old - solution's VDF step is not greater than parent's VDF step
//! 2. Valid solution reuse - parent changes but solution remains valid
//! 3. Below difficulty - solution hash does not clear the parent's difficulty

use irys_actors::{
    BlockProdStrategy, BlockProducerInner, InvalidReason, ParentCheckResult, ProductionStrategy,
    async_trait,
};
use irys_types::{H256, NodeConfig, U256, block_production::SolutionContext};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tracing::info;

use crate::utils::{IrysNodeTest, solution_context};

/// Strategy that can pause and resume block production for testing.
struct TrackingStrategy {
    prod: ProductionStrategy,
    /// Signal when block production starts
    pause_signal: Mutex<Option<oneshot::Sender<()>>>,
    /// Signal to resume block production
    resume_signal: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl BlockProdStrategy for TrackingStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn fully_produce_new_block(
        &self,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<irys_types::SealedBlock>, reth::payload::EthBuiltPayload)>> {
        // Signal that we're starting and wait for resume
        if let Some(pause_tx) = self.pause_signal.lock().await.take() {
            let _ = pause_tx.send(());
        }

        if let Some(resume_rx) = self.resume_signal.lock().await.take() {
            let _ = resume_rx.await;
        }

        // Continue with normal production - this will check validity
        let result = self.prod.fully_produce_new_block(solution).await?;

        Ok(result)
    }
}

/// Test that solutions are discarded when VDF becomes too old.
///
/// This test verifies that when the parent chain advances and the solution's
/// VDF step is no longer greater than the parent's VDF step, the solution
/// is correctly discarded.
#[test_log::test(tokio::test)]
async fn heavy_solution_discarded_vdf_too_old() -> eyre::Result<()> {
    // Setup
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 4;

    let peer_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&peer_signer]);

    // Start nodes
    let node1 = IrysNodeTest::new_genesis(config.clone()).start().await;
    let node2 = node1.testing_peer_with_assignments(&peer_signer).await?;

    // Mine initial blocks
    for _i in 1..=2 {
        let block = node1.mine_block().await?;
        node2.wait_for_block_at_height(block.height, 10).await?;
    }

    // Create tracking strategy with pause/resume
    let (pause_tx, pause_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();

    let tracking_strategy = Arc::new(TrackingStrategy {
        prod: ProductionStrategy {
            inner: node1.node_ctx.block_producer_inner.clone(),
        },
        pause_signal: Mutex::new(Some(pause_tx)),
        resume_signal: Mutex::new(Some(resume_rx)),
    });

    // Generate solution at current VDF step
    let solution = solution_context(&node1.node_ctx).await?;
    let original_vdf = solution.vdf_step;

    // Start block production (will pause)
    let strategy_clone = tracking_strategy.clone();
    let sol_clone = solution.clone();
    let handle =
        tokio::spawn(async move { strategy_clone.fully_produce_new_block(sol_clone).await });

    // Wait for production to pause
    pause_rx.await?;

    // Mine blocks until solution becomes invalid (solution.vdf_step <= parent.vdf_step)
    // Get initial VDF by mining a block and checking its parent
    let initial_block = node2.mine_block().await?;
    node2
        .wait_for_block_at_height(initial_block.height, 10)
        .await?;
    let mut node2_latest_vdf = initial_block.vdf_limiter_info.global_step_number;
    let mut block_count = 1;

    while node2_latest_vdf < solution.vdf_step {
        let block = node2.mine_block().await?;
        node2_latest_vdf = block.vdf_limiter_info.global_step_number;
        block_count += 1;

        info!(
            "Node2 mined block {} - VDF: {} (need >= {} to invalidate solution)",
            block_count, node2_latest_vdf, solution.vdf_step
        );

        node2.wait_for_block_at_height(block.height, 10).await?;

        // Safety limit to prevent infinite loop in case of test issues
        if block_count >= 50 {
            panic!(
                "Mining took too many blocks ({}), test may have issue",
                block_count
            );
        }
    }

    // Verify we've reached the invalidation point
    assert!(
        node2_latest_vdf >= solution.vdf_step,
        "Should have mined until VDF {} >= solution VDF {}",
        node2_latest_vdf,
        solution.vdf_step
    );
    info!(
        "Successfully mined {} blocks to invalidate solution (VDF {} >= solution VDF {})",
        block_count, node2_latest_vdf, solution.vdf_step
    );

    // Wait for node1 to receive all of node2's blocks via gossip before resuming,
    // so node1 sees the advanced chain state and correctly rejects the stale solution
    let node2_height = node2.get_canonical_chain_height().await;
    node1.wait_for_block_at_height(node2_height, 10).await?;

    // Resume block production
    resume_tx.send(()).unwrap();

    // Wait for block production
    let result = handle.await;
    let production_result = result??;

    // Should be None because VDF is too old
    assert!(
        production_result.is_none(),
        "Expected None when VDF too old. Original VDF: {}, but chain advanced significantly",
        original_vdf
    );

    // Cleanup
    node1.stop().await;
    node2.stop().await;
    Ok(())
}

/// Test that solutions below the parent's difficulty are discarded during
/// production rather than built and shipped to pre-validation.
///
/// A miner can surface a solution that clears its own difficulty target but
/// falls below the canonical parent's `diff` (e.g. the miner's target lags the
/// tip, or a higher-difficulty parent became canonical mid-production). Without
/// the producer-side gate, such a solution is built into a block and only
/// rejected at terminal pre-validation (`SolutionHashBelowDifficulty`), whose
/// error panics the block producer service. The gate must discard it early
/// (`Ok(None)`), so it is never built.
#[test_log::test(tokio::test)]
async fn heavy_solution_discarded_below_difficulty() -> eyre::Result<()> {
    // Setup
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 4;

    let node = IrysNodeTest::new_genesis(config).start().await;

    // Mine a couple of blocks so we have a stable parent and an advancing VDF.
    for _ in 0..2 {
        let block = node.mine_block().await?;
        node.wait_for_block_at_height(block.height, 10).await?;
    }

    // The parent the producer will build on, and its difficulty.
    let prev_block = {
        let read = node.node_ctx.block_tree_guard.read();
        let parent_hash = read.get_max_cumulative_difficulty_block().1;
        read.get_block(&parent_hash)
            .cloned()
            .expect("parent block present in block tree")
    };
    assert!(
        prev_block.diff > U256::zero(),
        "test premise requires a non-zero parent difficulty"
    );

    // Start from a real, otherwise-valid solution. Retry until its VDF step is
    // strictly greater than the parent's, so the VDF-too-old gate cannot be the
    // reason for a discard — otherwise this test would pass without exercising
    // the difficulty gate at all.
    let mut solution = solution_context(&node.node_ctx).await?;
    let mut attempts = 0;
    while solution.vdf_step <= prev_block.vdf_limiter_info.global_step_number {
        attempts += 1;
        assert!(
            attempts < 30,
            "VDF did not advance past parent step {} after {} attempts",
            prev_block.vdf_limiter_info.global_step_number,
            attempts
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        solution = solution_context(&node.node_ctx).await?;
    }

    // Force the solution hash just below the parent difficulty. Crafting
    // `diff - 1` in little-endian (the same interpretation the validator uses
    // via `hash_to_number`/`u256_from_le_bytes`) guarantees
    // `hash_to_number(solution_hash) < parent.diff` while every other field
    // stays genuine.
    let below_difficulty = prev_block.diff - U256::one();
    solution.solution_hash = H256(below_difficulty.to_le_bytes());

    let strategy = ProductionStrategy {
        inner: node.node_ctx.block_producer_inner.clone(),
    };

    // Must be discarded, not built. Without the gate this returns `Err`
    // (terminal pre-validation failure), which the service turns into a panic.
    let result = strategy.fully_produce_new_block(solution).await?;
    assert!(
        result.is_none(),
        "a sub-difficulty solution must be discarded (Ok(None)), not built"
    );

    node.stop().await;
    Ok(())
}

/// Directly exercises the parent-change rebuild guard: a solution that was valid
/// for the parent it was built on, but whose hash no longer clears a
/// *higher-difficulty* tip that became canonical mid-production. The guard must
/// report `SolutionInvalid { BelowDifficulty }` (so the candidate loop discards
/// it) rather than `MustRebuild` it into a block that can only fail its own
/// pre-validation.
#[test_log::test(tokio::test)]
async fn heavy_rebuild_guard_rejects_below_difficulty_parent() -> eyre::Result<()> {
    // Setup
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 4;

    let node = IrysNodeTest::new_genesis(config).start().await;

    // Mine so the tip has a real parent (height 2 on parent height 1).
    for _ in 0..2 {
        let block = node.mine_block().await?;
        node.wait_for_block_at_height(block.height, 10).await?;
    }

    // The current canonical tip plays the role of the higher-difficulty parent
    // that became canonical while a block was being produced on its parent.
    let tip = {
        let read = node.node_ctx.block_tree_guard.read();
        let tip_hash = read.get_max_cumulative_difficulty_block().1;
        read.get_block(&tip_hash)
            .cloned()
            .expect("tip block present in block tree")
    };
    assert!(tip.diff > U256::one(), "test premise requires tip diff > 1");

    // A solution the producer was building on the tip's parent: still ahead on
    // VDF (so the VDF-too-old branch cannot be the reason), but with a hash that
    // falls below the tip's difficulty.
    let mut solution = solution_context(&node.node_ctx).await?;
    solution.vdf_step = tip.vdf_limiter_info.global_step_number + 1;
    solution.solution_hash = H256((tip.diff - U256::one()).to_le_bytes());

    let strategy = ProductionStrategy {
        inner: node.node_ctx.block_producer_inner.clone(),
    };

    // Evaluate against the tip's parent, so the guard sees the tip as a *changed*,
    // higher-difficulty parent.
    let result = strategy
        .check_parent_and_solution_validity(&tip.previous_block_hash, &solution)
        .await;

    assert!(
        matches!(
            result,
            ParentCheckResult::SolutionInvalid {
                reason: InvalidReason::BelowDifficulty { .. },
                ..
            }
        ),
        "expected SolutionInvalid::BelowDifficulty for a higher-difficulty new parent, got {:?}",
        result
    );

    node.stop().await;
    Ok(())
}

/// Test that solutions are reused when parent changes but remains valid.
///
/// This test verifies that when a parent block changes during production,
/// but the solution still meets all requirements (VDF step and difficulty),
/// the block producer rebuilds on the new parent using the same solution.
///
/// To avoid a VDF timing race (where the competing block's VDF step could
/// exceed the solution's), we mine node2's block first (without gossip),
/// then generate the solution on node1 — guaranteeing the solution's VDF
/// step is strictly greater. The gossiped block triggers the parent change.
#[test_log::test(tokio::test)]
async fn heavy_solution_reused_when_parent_changes_but_valid() -> eyre::Result<()> {
    info!("Starting test: solution reused when parent changes but remains valid");

    // Setup
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 4;

    let peer_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&peer_signer]);

    // Start nodes
    let node1 = IrysNodeTest::new_genesis(config.clone()).start().await;
    let node2 = node1.testing_peer_with_assignments(&peer_signer).await?;

    // Mine initial blocks
    for _ in 0..2 {
        let block = node1.mine_block().await?;
        node1.wait_for_block_at_height(block.height, 10).await?;
        node2.wait_for_block_at_height(block.height, 10).await?;
    }

    // Mine ONE block on node2 WITHOUT gossip. This block will later cause
    // a parent change on node1 when gossiped, after production is paused.
    let (node2_block, _eth_payload, _txs) = node2.mine_block_without_gossip().await?;
    let node2_block_vdf = node2_block.vdf_limiter_info.global_step_number;
    info!(
        "Node2 mined block at height {} with VDF step {} (not yet gossiped)",
        node2_block.height, node2_block_vdf
    );

    // Generate solution on node1, retrying until its VDF step is strictly
    // greater than node2's block VDF step. This guarantees the solution
    // remains valid when node2's block becomes the new parent.
    let mut solution = solution_context(&node1.node_ctx).await?;
    let mut retry_count = 0;
    while solution.vdf_step <= node2_block_vdf {
        retry_count += 1;
        assert!(
            retry_count < 30,
            "Failed to generate solution with VDF step > {} after {} attempts",
            node2_block_vdf,
            retry_count
        );
        info!(
            "Solution VDF {} not > node2 block VDF {}, advancing VDF (attempt {})...",
            solution.vdf_step, node2_block_vdf, retry_count
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        solution = solution_context(&node1.node_ctx).await?;
    }
    let original_solution_hash = solution.solution_hash;
    let original_vdf_step = solution.vdf_step;

    info!(
        "Generated solution - hash: {}, VDF step: {} (> node2 block VDF {})",
        original_solution_hash, original_vdf_step, node2_block_vdf
    );

    // Create tracking strategy
    let (pause_tx, pause_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();

    let tracking_strategy = Arc::new(TrackingStrategy {
        prod: ProductionStrategy {
            inner: node1.node_ctx.block_producer_inner.clone(),
        },
        pause_signal: Mutex::new(Some(pause_tx)),
        resume_signal: Mutex::new(Some(resume_rx)),
    });

    // Start block production on node1 (will pause)
    let strategy_clone = tracking_strategy.clone();
    let sol_clone = solution.clone();
    let handle =
        tokio::spawn(async move { strategy_clone.fully_produce_new_block(sol_clone).await });

    // Wait for production to start and pause
    pause_rx.await?;
    info!("Node1 paused, gossiping node2's block to trigger parent change");

    // Gossip node2's block to node1 to trigger a parent change.
    node2.gossip_block_to_peers(&node2_block)?;

    // Wait for node1 to process node2's block
    node1
        .wait_for_block_at_height(node2_block.height, 10)
        .await?;

    // Resume node1's block production
    info!("Resuming node1 block production");
    resume_tx.send(()).unwrap();

    // Get the result
    let result = handle.await??;
    let (block, _eth_payload) = result.expect("Block should be produced successfully");

    // Verify the block was built on node2's block (parent changed)
    assert_eq!(
        block.header().previous_block_hash,
        node2_block.block_hash,
        "Block should be built on the new parent"
    );

    // Verify same solution hash was used
    assert_eq!(
        block.header().solution_hash,
        original_solution_hash,
        "Same solution hash should be reused after parent change"
    );

    info!("SUCCESS: Solution was reused when parent changed but remained valid");
    info!("Original solution hash: {}", original_solution_hash);
    info!("Block built on new parent: {}", node2_block.block_hash);
    info!("Final block height: {}", block.header().height);

    // Cleanup
    node1.stop().await;
    node2.stop().await;

    Ok(())
}
