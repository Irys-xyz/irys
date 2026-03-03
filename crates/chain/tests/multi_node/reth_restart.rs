use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;
use reth::rpc::types::BlockNumberOrTag;

const WAIT_SECS: u64 = 30;
const INITIAL_MINED_BLOCKS: u64 = 5;
const EXTRA_MINED_BLOCKS: u64 = 5;

// Scenario: a single node mines a short chain, extends it beyond the migration/prune depths, stops,
// and restarts without external sync. After the restart we assert that Reth rebuilds its
// latest/safe/finalized anchors purely from the block index using the expected migration and prune
// depths. We then mine one extra block to confirm that the head advances while the safe/finalized
// anchors stay pinned to the initial blocks from the block index.
#[test_log::test(tokio::test)]
async fn heavy_reth_restarts_use_block_index_before_sync() -> eyre::Result<()> {
    let genesis_config = NodeConfig::testing().with_consensus(|cons| {
        cons.block_migration_depth = 2;
        cons.block_tree_depth = 3;
        cons.epoch.num_blocks_in_epoch = 5;
        cons.chunk_size = 32;
    });

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_with_name("GENESIS")
        .await;

    let migration_depth = genesis_node.node_ctx.config.consensus.block_migration_depth as u64;
    let prune_depth = genesis_node.node_ctx.config.consensus.block_tree_depth;

    genesis_node
        .mine_blocks(INITIAL_MINED_BLOCKS as usize)
        .await?;

    let shared_tip_height = genesis_node.get_canonical_chain_height().await;
    let shared_tip = genesis_node.get_block_by_height(shared_tip_height).await?;

    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            shared_tip.evm_block_hash,
            WAIT_SECS,
        )
        .await?;

    genesis_node
        .mine_blocks(EXTRA_MINED_BLOCKS as usize)
        .await?;

    let final_height = genesis_node.get_canonical_chain_height().await;
    assert_eq!(
        final_height,
        shared_tip_height + EXTRA_MINED_BLOCKS,
        "node should reflect the additional blocks mined before the restart",
    );

    let genesis_head = genesis_node.get_block_by_height(final_height).await?;
    let safe_height = final_height.saturating_sub(migration_depth);
    let genesis_safe_block = genesis_node
        .wait_for_block_in_index(safe_height, false, WAIT_SECS as usize)
        .await?;
    let finalized_height = final_height.saturating_sub(prune_depth);
    let genesis_finalized_block = genesis_node
        .wait_for_block_in_index(finalized_height, false, WAIT_SECS as usize)
        .await?;

    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            genesis_head.evm_block_hash,
            WAIT_SECS,
        )
        .await?;
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Safe,
            genesis_safe_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Finalized,
            genesis_finalized_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;

    assert_ne!(
        genesis_head.evm_block_hash,
        genesis_safe_block.evm_block_hash
    );
    assert_ne!(
        genesis_finalized_block.evm_block_hash,
        genesis_safe_block.evm_block_hash
    );
    let genesis_node = genesis_node.stop().await.start().await;
    tracing::error!(evm.hash = ?genesis_safe_block.evm_block_hash);
    tracing::error!(evm.hash = ?genesis_head.evm_block_hash);

    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            // latest block becomes equal to the safe block on restart
            genesis_safe_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Safe,
            genesis_safe_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Finalized,
            genesis_finalized_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;

    // Mine one more block post-restart and ensure Reth advances only the head.
    tracing::error!("mining");
    let new_head = genesis_node.mine_block().await?;
    genesis_node
        .wait_for_reth_marker(BlockNumberOrTag::Latest, new_head.evm_block_hash, WAIT_SECS)
        .await?;
    // the other tags remain as is
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Safe,
            genesis_safe_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;
    genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Finalized,
            genesis_finalized_block.evm_block_hash,
            WAIT_SECS,
        )
        .await?;

    Ok(())
}
