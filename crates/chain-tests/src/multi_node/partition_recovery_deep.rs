use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{NodeConfig, UnixTimestamp, hardfork_config::Cascade};
use tracing::{error, info};

/// Tests that `recover_from_network_partition()` correctly rolls back block index,
/// supply state, and storage module data when a deep reorg exceeds
/// `block_migration_depth`.
///
/// This exercises the code path where `ForkChoiceMarkers` is computed before
/// recovery truncates the block index (stale markers). The test verifies that
/// despite the stale markers, `migrate_blocks()` succeeds and the node continues
/// operating correctly after recovery.
///
/// Timeline:
///   Blocks 1–4  (gossip ON)  — shared chain, epoch boundaries at 2 and 4
///   Blocks 5–6  (gossip OFF) — genesis mines 2 blocks (minority fork, triggers recovery)
///   Blocks 5–7  (gossip OFF) — peer mines 3 blocks (majority fork)
///   Gossip ON   — peer's fork gossiped to genesis, triggers deep reorg
///
/// Verifies on genesis after reorg:
///   - Block index contains winning fork blocks, not orphaned ones
///   - Chain linkage is continuous across the recovery boundary
///   - Supply state cumulative_emitted matches new canonical chain
///   - Partition assignments exist and are valid
///   - Node can continue mining and migrating blocks
#[test_log::test(tokio::test)]
async fn heavy4_partition_recovery_deep() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    // migration_depth=1 so that 2+ orphaned fork blocks trigger recovery
    let block_migration_depth: u32 = 1;
    let chunk_size: u64 = 32;
    let num_blocks_in_epoch = 2;

    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.num_partitions_per_term_ledger_slot = 1;
        c.epoch.num_blocks_in_epoch = num_blocks_in_epoch;
        c.block_migration_depth = block_migration_depth;
        c.entropy_packing_iterations = 1_000;
        c.genesis.initial_packed_partitions = Some(5.0);
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    genesis_config.storage.num_writes_before_sync = 1;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // ─── Phase 0: Start nodes and build shared base ───

    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 10)?;
    let genesis_node = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_addr = genesis_node.cfg.miner_address();

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_test = IrysNodeTest::new(peer_config);
    StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), 10)?;
    let peer_node = peer_test
        .start_and_wait_for_packing("PEER", seconds_to_wait)
        .await;

    IrysNodeTest::announce_between(&genesis_node, &peer_node).await?;

    // Peer only follows during shared base
    peer_node.stop_mining();

    // Mine shared base: blocks 1-4 (epoch boundaries at 2 and 4)
    for h in 1..=4_u64 {
        genesis_node.mine_block().await?;
        let block = genesis_node.get_block_by_height(h).await?;
        peer_node
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
        info!(height = h, "Both nodes synced");

        // Wait for packing after epoch boundaries
        if h % num_blocks_in_epoch == 0 {
            genesis_node.wait_for_packing(seconds_to_wait).await;
            peer_node.wait_for_packing(seconds_to_wait).await;
        }
    }
    info!("Shared base complete at height 4");

    // Wait for block index to catch up: height 4 - migration_depth = 3
    let shared_index_height = 4 - block_migration_depth as u64; // = 3
    genesis_node
        .wait_until_block_index_height(shared_index_height, seconds_to_wait)
        .await?;
    info!(
        index_height = genesis_node.get_block_index_height(),
        "Genesis block index after shared base"
    );

    // ─── Phase 1: Fork — each node mines independently ───

    genesis_node.gossip_disable();
    peer_node.gossip_disable();
    error!("Gossip disabled, fork point at height 4");

    // Re-enable mining on peer
    peer_node.node_ctx.start_mining()?;
    peer_node.wait_for_packing(seconds_to_wait).await;

    // Genesis mines 2 blocks (minority fork)
    // With migration_depth=1, block index advances to height 5 (6-1=5)
    // When reorg happens, old_fork_blocks(2) > migration_depth(1) triggers recovery
    for i in 0..2 {
        let (block, _, _) = genesis_node.mine_block_without_gossip().await?;
        error!(
            "Genesis mined fork block {} at height {}: {}",
            i + 1,
            block.height,
            block.block_hash
        );
    }
    let genesis_fork_height = 6_u64;
    genesis_node
        .wait_until_height(genesis_fork_height, seconds_to_wait)
        .await?;

    // Wait for genesis block index to advance on the minority fork
    let genesis_fork_index = genesis_fork_height - block_migration_depth as u64; // = 5
    genesis_node
        .wait_until_block_index_height(genesis_fork_index, seconds_to_wait)
        .await?;
    error!(
        "Genesis block index at height {} (from minority fork)",
        genesis_node.get_block_index_height()
    );

    // Peer mines 3 blocks (majority fork — longer)
    let mut peer_fork_blocks = Vec::new();
    for i in 0..3 {
        let (block, _, _) = peer_node.mine_block_without_gossip().await?;
        error!(
            "Peer mined fork block {} at height {}: {}",
            i + 1,
            block.height,
            block.block_hash
        );
        peer_fork_blocks.push(block);
    }
    let peer_fork_height = 7_u64;
    info!(
        peer_height = peer_fork_height,
        genesis_height = genesis_fork_height,
        "Peer fork is longer"
    );

    // ─── Phase 2: Trigger reorg ───

    genesis_node.gossip_enable();
    peer_node.gossip_enable();
    IrysNodeTest::announce_between(&genesis_node, &peer_node).await?;
    error!("Gossip re-enabled, gossiping peer's fork to trigger deep reorg");

    for block in &peer_fork_blocks {
        peer_node.gossip_block_to_peers(block)?;
        genesis_node
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }

    genesis_node
        .wait_until_height(peer_fork_height, seconds_to_wait)
        .await?;
    let adopted_height = genesis_node.get_canonical_chain_height().await;
    error!(adopted_height, "Genesis adopted peer's chain");

    // Wait for re-migration on genesis
    let new_expected_index = peer_fork_height - block_migration_depth as u64; // = 6
    genesis_node
        .wait_until_block_index_height(new_expected_index, seconds_to_wait)
        .await?;

    // ─── Phase 3: Verify block index consistency ───

    let index_height = genesis_node.get_block_index_height();
    assert!(
        index_height >= new_expected_index,
        "Expected block index height >= {new_expected_index}, got {index_height}"
    );

    // Verify blocks in the index are from the winning fork, not the orphaned fork
    for h in 5..=index_height {
        let indexed_block = genesis_node.get_block_by_height_from_index(h, false)?;
        let canonical_block = genesis_node.get_block_by_height(h).await?;
        assert_eq!(
            indexed_block.block_hash, canonical_block.block_hash,
            "Block index at height {h} should contain the winning fork's block, \
             not the orphaned fork's"
        );
    }
    info!("Block index verified: all entries from winning fork");

    // Verify chain linkage across the recovery boundary (height 4 → height 5)
    let block_at_fork_point = genesis_node.get_block_by_height_from_index(4, false)?;
    let block_after_recovery = genesis_node.get_block_by_height_from_index(5, false)?;
    assert_eq!(
        block_after_recovery.previous_block_hash, block_at_fork_point.block_hash,
        "Block at height 5 should link to the fork point block at height 4"
    );
    info!("Chain linkage verified across recovery boundary");

    // ─── Phase 4: Verify supply state ───

    let supply_after = genesis_node
        .node_ctx
        .supply_state_guard
        .as_ref()
        .expect("supply state not set")
        .get();

    let mut expected_emitted = irys_types::U256::zero();
    for h in 1..=index_height {
        let block = genesis_node.get_block_by_height_from_index(h, false)?;
        expected_emitted = expected_emitted.saturating_add(block.reward_amount);
    }
    assert_eq!(
        supply_after.cumulative_emitted, expected_emitted,
        "Supply state should match new canonical chain's cumulative rewards \
         (rollback of orphaned rewards + re-migration of winning fork)"
    );
    info!("Supply state verified");

    // ─── Phase 5: Verify storage modules ───

    let genesis_assignments = genesis_node.get_partition_assignments(genesis_addr);
    assert!(
        !genesis_assignments.is_empty(),
        "Genesis should have partition assignments after recovery"
    );

    {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        let assigned_count = sms
            .iter()
            .filter(|sm| sm.partition_assignment().is_some())
            .count();
        assert!(
            assigned_count > 0,
            "At least some storage modules should be assigned after recovery"
        );
        info!(
            assigned_count,
            total = sms.len(),
            "Storage modules after recovery"
        );
    }

    // ─── Phase 6: Verify continued operation ───
    // If FCU/Reth state is inconsistent, block production will fail here.

    genesis_node.mine_block().await?;
    peer_node
        .wait_until_height(peer_fork_height + 1, seconds_to_wait)
        .await?;
    info!("Block 8 mined and synced — mining continues after recovery");

    genesis_node.mine_block().await?;
    peer_node
        .wait_until_height(peer_fork_height + 2, seconds_to_wait)
        .await?;
    info!("Block 9 mined and synced — system fully operational");

    // Verify block index continues to advance
    let final_index_height = genesis_node.get_block_index_height();
    assert!(
        final_index_height > peer_fork_height,
        "Block index should continue advancing after recovery, got {final_index_height}"
    );

    // Verify supply state remains consistent after continued mining
    let final_supply = genesis_node
        .node_ctx
        .supply_state_guard
        .as_ref()
        .expect("supply state not set")
        .get();
    let mut final_expected = irys_types::U256::zero();
    for h in 1..=final_index_height {
        let block = genesis_node.get_block_by_height_from_index(h, false)?;
        final_expected = final_expected.saturating_add(block.reward_amount);
    }
    assert_eq!(
        final_supply.cumulative_emitted, final_expected,
        "Supply state should remain consistent after continued mining"
    );
    info!("Final supply state verified — all checks passed");

    // ─── Phase 7: Cleanup ───

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
