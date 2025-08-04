use rust_decimal_macros::dec;

use crate::utils::IrysNodeTest;

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
