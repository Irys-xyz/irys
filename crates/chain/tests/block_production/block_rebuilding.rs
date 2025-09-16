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
//!
//! NOTE: All tests use the `serial_` prefix to ensure they run sequentially.
//! This is required because the VDF thread needs consistent CPU time without
//! OS scheduling congestion from concurrent test execution.

use irys_actors::{async_trait, BlockProdStrategy, BlockProducerInner, ProductionStrategy};
use irys_types::{block_production::SolutionContext, IrysBlockHeader, NodeConfig, H256};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tracing::info;

use crate::utils::{solution_context, IrysNodeTest};

/// Strategy that can pause block production and track various metrics for testing.
struct TrackingStrategy {
    prod: ProductionStrategy,
    /// Signal when block production starts
    pause_signal: Mutex<Option<oneshot::Sender<()>>>,
    /// Signal to resume block production
    resume_signal: Mutex<Option<oneshot::Receiver<()>>>,
    /// Track the solution hash
    solution_hash_tracked: Arc<Mutex<Option<H256>>>,
    /// Track the solution VDF step
    solution_vdf_tracked: Arc<Mutex<Option<u64>>>,
    /// Track if solution was used or discarded
    solution_used: Arc<Mutex<Option<bool>>>,
}

#[async_trait::async_trait]
impl BlockProdStrategy for TrackingStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn fully_produce_new_block(
        &self,
        solution: SolutionContext,
    ) -> eyre::Result<Option<(Arc<IrysBlockHeader>, reth::payload::EthBuiltPayload)>> {
        // Track the solution hash and VDF step
        *self.solution_hash_tracked.lock().await = Some(solution.solution_hash);
        *self.solution_vdf_tracked.lock().await = Some(solution.vdf_step);

        // Signal that we're starting and wait for resume
        if let Some(pause_tx) = self.pause_signal.lock().await.take() {
            let _ = pause_tx.send(());
        }

        if let Some(resume_rx) = self.resume_signal.lock().await.take() {
            let _ = resume_rx.await;
        }

        // Continue with normal production - this will check validity
        let result = self.prod.fully_produce_new_block(solution).await?;

        // Track whether solution was used (Some result) or discarded (None)
        *self.solution_used.lock().await = Some(result.is_some());

        Ok(result)
    }
}

/// Test that solutions are discarded when VDF becomes too old.
///
/// This test verifies that when the parent chain advances and the solution's
/// VDF step is no longer greater than the parent's VDF step, the solution
/// is correctly discarded.
#[test_log::test(actix::test)]
async fn serial_solution_discarded_vdf_too_old() -> eyre::Result<()> {
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
        node2.wait_until_height(block.height, 10).await?;
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
        solution_hash_tracked: Arc::new(Mutex::new(None)),
        solution_vdf_tracked: Arc::new(Mutex::new(None)),
        solution_used: Arc::new(Mutex::new(None)),
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
    node2.wait_until_height(initial_block.height, 10).await?;
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

        node2.wait_until_height(block.height, 10).await?;

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

/// Test that solutions are reused when parent changes but remains valid.
///
/// This test verifies that when a parent block changes during production,
/// but the solution still meets all requirements (VDF step and difficulty),
/// the block producer rebuilds on the new parent using the same solution.
#[test_log::test(actix::test)]
async fn serial_solution_reused_when_parent_changes_but_valid() -> eyre::Result<()> {
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
        node1.wait_until_height(block.height, 10).await?;
        node2.wait_until_height(block.height, 10).await?;
    }

    // Create tracking strategy
    let (pause_tx, pause_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();

    let tracking_strategy = Arc::new(TrackingStrategy {
        prod: ProductionStrategy {
            inner: node1.node_ctx.block_producer_inner.clone(),
        },
        pause_signal: Mutex::new(Some(pause_tx)),
        resume_signal: Mutex::new(Some(resume_rx)),
        solution_hash_tracked: Arc::new(Mutex::new(None)),
        solution_vdf_tracked: Arc::new(Mutex::new(None)),
        solution_used: Arc::new(Mutex::new(None)),
    });

    // Generate solution
    let solution = solution_context(&node1.node_ctx).await?;
    let original_solution_hash = solution.solution_hash;
    let original_vdf_step = solution.vdf_step;

    info!(
        "Generated solution - hash: {}, VDF step: {}",
        original_solution_hash, original_vdf_step
    );

    // Start block production (will pause)
    let strategy_clone = tracking_strategy.clone();
    let sol_clone = solution.clone();
    let handle =
        tokio::spawn(async move { strategy_clone.fully_produce_new_block(sol_clone).await });

    // Wait for production to start
    pause_rx.await?;
    info!("Node1 paused, node2 will mine a block");

    // Node2 mines ONE block (not too many to keep solution valid)
    let node2_block = node2.mine_block().await?;
    info!(
        "Node2 mined block at height {} with VDF step {}",
        node2_block.height, node2_block.vdf_limiter_info.global_step_number
    );

    // Ensure both nodes see the new block
    node2.wait_until_height(node2_block.height, 10).await?;
    node1.wait_until_height(node2_block.height, 10).await?;

    // Verify solution is still valid for new parent
    assert!(
        original_vdf_step > node2_block.vdf_limiter_info.global_step_number,
        "Solution VDF {} should be > new parent VDF {}",
        original_vdf_step,
        node2_block.vdf_limiter_info.global_step_number
    );

    // Resume node1's block production
    info!("Resuming node1 block production");
    resume_tx.send(()).unwrap();

    // Get the result
    let result = handle.await??;
    let (block, _eth_payload) = result.expect("Block should be produced successfully");

    // Verify the block was built on node2's block (parent changed)
    assert_eq!(
        block.previous_block_hash, node2_block.block_hash,
        "Block should be built on the new parent"
    );

    // Verify same solution hash was used
    assert_eq!(
        block.solution_hash, original_solution_hash,
        "Same solution hash should be reused after parent change"
    );

    // Verify the stored solution hash matches
    let stored_hash = tracking_strategy.solution_hash_tracked.lock().await;
    assert_eq!(
        stored_hash.as_ref().unwrap(),
        &original_solution_hash,
        "Strategy should have tracked the original solution hash"
    );

    info!("SUCCESS: Solution was reused when parent changed but remained valid");
    info!("Original solution hash: {}", original_solution_hash);
    info!("Block built on new parent: {}", node2_block.block_hash);
    info!("Final block height: {}", block.height);

    // Verify both nodes have validated the newly produced block
    info!(
        "Waiting for both nodes to validate the new block at height {}",
        block.height
    );
    node1.wait_until_height(block.height, 10).await?;
    node2.wait_until_height(block.height, 10).await?;
    info!(
        "Both nodes have successfully validated the block at height {}",
        block.height
    );

    // Cleanup
    node1.stop().await;
    node2.stop().await;

    Ok(())
}
