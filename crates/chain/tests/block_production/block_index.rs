use tracing::info;

use crate::utils::IrysNodeTest;

/// mine 10 blocks, check that they get migrated to the database.
/// ensure that the poa chuk is present
#[test_log::test(tokio::test)]
async fn heavy3_mine_ten_blocks_with_migration_depth_two() -> eyre::Result<()> {
    // Configure a node with block_migration_depth = 2
    let mut config = irys_types::NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 2;

    let node = IrysNodeTest::new_genesis(config).start().await;

    // Collect mined blocks with their chunks
    let mut mined_blocks = Vec::new();
    let mut migrated_blocks = Vec::new();

    // Mine 10 blocks explicitly
    for i in 1..=10 {
        info!("Mining block {}", i);
        let block = node.mine_block().await?;

        info!(
            "Mined block at height {} with hash {:?}",
            block.height, block.block_hash
        );

        // Assert that the mined block has a chunk in its PoA
        assert!(
            block.poa.chunk.is_some(),
            "Mined block at height {} should have a chunk in poa",
            block.height
        );

        mined_blocks.push(block);

        // Verify block index migration at appropriate depth
        // With migration_depth = 2, block at height H-2 should be in the index when we mine block H
        if i >= 3 {
            let migration_height = i - 2;

            info!(
                "Verifying block at height {} is in block index (current height: {})",
                migration_height, i
            );

            // Wait for the block to appear in the index AND the DB
            let block_from_index = node
                .wait_for_block_in_index(migration_height, true, 10)
                .await?;

            // Assert that the block read from index has the chunk
            assert!(
                block_from_index.poa.chunk.is_some(),
                "Block at height {} read from index should have a chunk",
                migration_height
            );

            let expected_block = &mined_blocks[(migration_height - 1) as usize];
            assert_eq!(
                expected_block, &block_from_index,
                "Block hash in index should match the mined block at height {}",
                migration_height
            );
            migrated_blocks.push(block_from_index);
        }
    }

    info!("All blocks mined and validated. Restarting node to verify persistence...");

    // Restart the node to ensure blocks are persisted correctly
    let node = node.stop().await.start().await;

    // Verify all mined blocks can be retrieved from block pool after restart
    for migrated_block in migrated_blocks {
        info!(
            "Verifying block at height {} after restart",
            migrated_block.height
        );

        // Get the block from the block tree using the hash
        let block_from_block_pool = node
            .node_ctx
            .block_pool
            .get_block_header(&migrated_block.block_hash)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "Block at height {} should be retrievable after restart",
                    migrated_block.height
                )
            })
            .unwrap();

        // Assert that the block has the chunk
        assert!(
            block_from_block_pool.poa.chunk.is_some(),
            "Block at height {} should have a chunk after restart",
            migrated_block.height
        );

        // Assert that the block matches the originally mined block
        assert_eq!(
            &migrated_block,
            block_from_block_pool.as_ref(),
            "Block at height {} should match the mined block after restart",
            migrated_block.height
        );
    }

    info!("All blocks verified successfully after restart");

    node.stop().await;
    Ok(())
}
