//! Integration test: verifies that anchor validation rejects orphan-fork
//! blocks after a reorg by using `MigratedBlockHashes` as canonical authority.

use crate::utils::IrysNodeTest;
use irys_actors::anchor_validation::{get_anchor_height, get_canonical_anchor_height};
use irys_types::{H256, NodeConfig};
use std::sync::Arc;
use tracing::debug;

/// After a reorg, blocks from the losing fork should NOT be accepted as
/// canonical anchors. This test:
/// 1. Starts two mining peers that build competing forks
/// 2. Triggers a reorg by extending the losing peer's chain
/// 3. Phase 1 (in-tree): verifies that the orphan is rejected by `get_canonical_anchor_height`
///    but still findable with `get_anchor_height` while in the block tree
/// 4. Phase 2 (DB fallback): mines extra blocks to prune height 3 from the tree,
///    then verifies the orphan is rejected via DB cross-check and the canonical
///    block is found via MigratedBlockHashes
#[test_log::test(tokio::test)]
async fn heavy_test_anchor_rejects_orphan_after_reorg() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 15;
    let block_migration_depth = 1;
    // Small tree depth forces blocks at height 3 to be pruned once tip reaches ~7,
    // ensuring the assertions exercise the DB fallback path in get_canonical_anchor_height.
    let block_tree_depth = 3;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth;
    genesis_config.consensus.get_mut().block_tree_depth = block_tree_depth;

    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer1_config = genesis_node.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testing_peer_with_signer(&peer2_signer);

    let peer1_node = IrysNodeTest::new(peer1_config)
        .start_with_name("PEER1")
        .await;
    let peer2_node = IrysNodeTest::new(peer2_config)
        .start_with_name("PEER2")
        .await;

    let peer1_stake_tx = peer1_node.post_stake_commitment(None).await?;
    let peer1_pledge_tx = peer1_node.post_pledge_commitment(None).await?;
    let peer2_stake_tx = peer2_node.post_stake_commitment(None).await?;
    let peer2_pledge_tx = peer2_node.post_pledge_commitment(None).await?;

    genesis_node
        .wait_for_mempool(peer1_stake_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer1_pledge_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_stake_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_pledge_tx.id(), seconds_to_wait)
        .await?;

    genesis_node.mine_block().await?;
    genesis_node.mine_block().await?;

    genesis_node
        .wait_for_block_at_height(2, seconds_to_wait)
        .await?;
    genesis_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    peer1_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    let (result1, result2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1)
    );
    result1?;
    result2?;

    peer1_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    peer2_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;

    peer1_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await?;

    let peer1_block = peer1_node.get_block_by_height(3).await?;
    let peer2_block = peer2_node.get_block_by_height(3).await?;

    debug!(
        "peer1 block at height 3: {} c_diff: {}",
        peer1_block.block_hash, peer1_block.cumulative_diff
    );
    debug!(
        "peer2 block at height 3: {} c_diff: {}",
        peer2_block.block_hash, peer2_block.cumulative_diff
    );

    let peer1_block = Arc::new(peer1_block);
    let peer2_block = Arc::new(peer2_block);

    peer1_node.gossip_block_to_peers(&peer1_block)?;
    peer2_node.gossip_block_to_peers(&peer2_block)?;

    peer1_node
        .wait_for_block(&peer2_block.block_hash, seconds_to_wait)
        .await?;
    peer2_node
        .wait_for_block(&peer1_block.block_hash, seconds_to_wait)
        .await?;

    genesis_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    genesis_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await?;

    let genesis_block_3 = genesis_node.get_block_by_height(3).await?;

    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);

    // Extend the losing peer's chain to trigger a reorg on genesis
    let (orphaned_hash, canonical_hash) = if genesis_block_3.block_hash == peer1_block.block_hash {
        debug!("Genesis chose peer1 at height 3, extending peer2 to trigger reorg");
        peer2_node.mine_block().await?;
        (peer1_block.block_hash, peer2_block.block_hash)
    } else {
        debug!("Genesis chose peer2 at height 3, extending peer1 to trigger reorg");
        peer1_node.mine_block().await?;
        (peer2_block.block_hash, peer1_block.block_hash)
    };

    let reorg_event = reorg_future.await?;

    debug!(
        "Reorg: fork_parent height={}, old_fork={} blocks, new_fork={} blocks",
        reorg_event.fork_parent.height,
        reorg_event.old_fork.len(),
        reorg_event.new_fork.len()
    );

    let old_fork_hashes: Vec<H256> = reorg_event
        .old_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();
    debug!("Orphaned block hashes: {:?}", old_fork_hashes);

    genesis_node
        .wait_for_block_at_height(4, seconds_to_wait)
        .await?;

    let block_tree = &genesis_node.node_ctx.block_tree_guard;
    let db = &genesis_node.node_ctx.db;

    // Phase 1: while the orphan is still in the block tree (pre-prune).
    // get_canonical_anchor_height should reject it (filters by canonical chain).
    let orphan_result = get_canonical_anchor_height(block_tree, db, orphaned_hash)?;
    assert_eq!(
        orphan_result, None,
        "orphaned fork block should not be accepted as canonical anchor"
    );

    // get_anchor_height finds the orphan via get_block() while it's still in the tree.
    let non_canonical_result = get_anchor_height(block_tree, db, orphaned_hash)?;
    assert!(
        non_canonical_result.is_some(),
        "orphaned block should still be found with get_anchor_height (in-tree)"
    );

    // Phase 2: mine extra blocks to prune height 3 from the tree (block_tree_depth=3
    // keeps only ~3 blocks from tip). This forces the remaining assertions through
    // the DB fallback path.
    genesis_node.mine_blocks(4).await?;
    genesis_node
        .wait_for_block_at_height(8, seconds_to_wait)
        .await?;

    // get_canonical_anchor_height: orphan was never migrated to MigratedBlockHashes,
    // so the DB cross-check correctly rejects it.
    let orphan_after_prune = get_canonical_anchor_height(block_tree, db, orphaned_hash)?;
    assert_eq!(
        orphan_after_prune, None,
        "orphaned block should be rejected via DB fallback after pruning"
    );

    // get_canonical_anchor_height: the winning fork's block was migrated and should be found via DB.
    let canonical_result = get_canonical_anchor_height(block_tree, db, canonical_hash)?;
    assert!(
        canonical_result.is_some(),
        "canonical block should be accepted as anchor via DB fallback"
    );

    peer2_node.stop().await;
    peer1_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
