use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::NodeConfig;
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

/// Tests that a reorg crossing two epoch boundaries results in the correct
/// canonical epoch state reflecting both epoch transitions on the winning fork.
///
/// This exercises the code path at `block_tree_service.rs` line 718 where
/// `.find()` returns only the first epoch block in `new_fork`. The block tree
/// cache maintains the correct epoch snapshot at the tip regardless, but this
/// test validates end-to-end correctness across multi-epoch reorgs.
///
/// Timeline:
///   Blocks 1–2  (gossip ON)  — shared chain, peer stakes+pledges, epoch 1 boundary
///   Blocks 3–6  (gossip OFF) — each node posts 2 pledges, mines through epochs 2 & 3
///   Block 7     (gossip OFF) — peer extends fork to be longer
///   Gossip ON   — peer's fork gossiped to genesis, triggers reorg
///
/// Verifies on genesis after reorg:
///   - epoch_snapshot.epoch_height == 6 (final epoch on winning fork)
///   - Genesis fork-only pledges (2, 3) absent from commitment state
///   - Peer fork-only pledges (2, 3) present in commitment state
///   - Genesis assignments revert (7 → 5)
///   - Peer assignments advance (1 → 3)
///   - System continues mining after reorg (not stuck)
#[test_log::test(tokio::test)]
async fn heavy4_partition_recovery_multi_epoch() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 10;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // ─── Phase 0: Start nodes ───

    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 5)?;
    let genesis_node = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_addr = genesis_node.cfg.miner_address();

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    // ─── Phase 1: Shared chain — peer stakes and pledges ───

    let peer_stake_tx = peer_node.post_stake_commitment(None).await?;
    let peer_pledge_tx = peer_node.post_pledge_commitment(None).await?;

    genesis_node
        .wait_for_mempool(peer_stake_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer_pledge_tx.id(), seconds_to_wait)
        .await?;

    // Block 1: includes peer's stake + pledge
    genesis_node.mine_block().await?;
    peer_node.wait_until_height(1, seconds_to_wait).await?;

    // Block 2: EPOCH 1 boundary → assigns partition hashes
    genesis_node.mine_block().await?;
    peer_node.wait_until_height(2, seconds_to_wait).await?;
    info!("Both nodes synced to height 2 (epoch 1 boundary)");

    genesis_node.wait_for_packing(seconds_to_wait).await;
    peer_node.wait_for_packing(seconds_to_wait).await;

    // Checkpoint: verify initial assignments
    let genesis_initial_assignments = genesis_node.get_partition_assignments(genesis_addr);
    let peer_initial_assignments = genesis_node.get_partition_assignments(peer_signer.address());
    info!(
        genesis_count = genesis_initial_assignments.len(),
        peer_count = peer_initial_assignments.len(),
        "Initial partition assignments"
    );
    assert_eq!(
        genesis_initial_assignments.len(),
        5,
        "Genesis should have 5 partition assignments from auto-pledges (5 submodules)"
    );
    assert_eq!(
        peer_initial_assignments.len(),
        1,
        "Peer should have 1 partition assignment"
    );

    let genesis_initial_partition_hashes: Vec<_> = genesis_initial_assignments
        .iter()
        .map(|pa| pa.partition_hash)
        .collect();

    // ─── Phase 2: Fork — each node posts 2 pledges across 2 epoch boundaries ───

    genesis_node.gossip_disable();
    peer_node.gossip_disable();
    error!("Gossip disabled, fork point at height 2");

    // --- Peer's fork (majority) ---

    // Peer pledge2: included in block 3b, assigned at epoch 2 (block 4b)
    let peer_pledge2 = peer_node
        .post_pledge_commitment_without_gossip(Some(peer_node.get_anchor().await?))
        .await?;
    info!(id = %peer_pledge2.id(), "Peer posted fork pledge2");

    let (peer_fork_3, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(3, seconds_to_wait).await?;
    error!(
        "Peer mined fork block 3b at height {}: {}",
        peer_fork_3.height, peer_fork_3.block_hash
    );

    let (peer_fork_4, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(4, seconds_to_wait).await?;
    error!(
        "Peer mined fork epoch block 4b at height {}: {}",
        peer_fork_4.height, peer_fork_4.block_hash
    );

    // Peer pledge3: included in block 5b, assigned at epoch 3 (block 6b)
    let peer_pledge3 = peer_node
        .post_pledge_commitment_without_gossip(Some(peer_node.get_anchor().await?))
        .await?;
    info!(id = %peer_pledge3.id(), "Peer posted fork pledge3");

    let (peer_fork_5, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(5, seconds_to_wait).await?;
    error!(
        "Peer mined fork block 5b at height {}: {}",
        peer_fork_5.height, peer_fork_5.block_hash
    );

    let (peer_fork_6, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(6, seconds_to_wait).await?;
    error!(
        "Peer mined fork epoch block 6b at height {}: {}",
        peer_fork_6.height, peer_fork_6.block_hash
    );

    // --- Genesis fork (minority) ---

    // Genesis pledge2: included in block 3a, assigned at epoch 2 (block 4a)
    let genesis_pledge2 = genesis_node
        .post_pledge_commitment_without_gossip(Some(genesis_node.get_anchor().await?))
        .await?;
    info!(id = %genesis_pledge2.id(), "Genesis posted fork pledge2");

    let (genesis_fork_3, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(3, seconds_to_wait).await?;
    error!(
        "Genesis mined fork block 3a at height {}: {}",
        genesis_fork_3.height, genesis_fork_3.block_hash
    );

    let (genesis_fork_4, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(4, seconds_to_wait).await?;
    error!(
        "Genesis mined fork epoch block 4a at height {}: {}",
        genesis_fork_4.height, genesis_fork_4.block_hash
    );

    // Genesis pledge3: included in block 5a, assigned at epoch 3 (block 6a)
    let genesis_pledge3 = genesis_node
        .post_pledge_commitment_without_gossip(Some(genesis_node.get_anchor().await?))
        .await?;
    info!(id = %genesis_pledge3.id(), "Genesis posted fork pledge3");

    let (genesis_fork_5, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(5, seconds_to_wait).await?;
    error!(
        "Genesis mined fork block 5a at height {}: {}",
        genesis_fork_5.height, genesis_fork_5.block_hash
    );

    let (genesis_fork_6, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(6, seconds_to_wait).await?;
    error!(
        "Genesis mined fork epoch block 6a at height {}: {}",
        genesis_fork_6.height, genesis_fork_6.block_hash
    );

    // Verify forks diverged
    assert_ne!(
        genesis_fork_3.block_hash, peer_fork_3.block_hash,
        "Fork blocks at height 3 should differ"
    );
    assert_ne!(
        genesis_fork_6.block_hash, peer_fork_6.block_hash,
        "Epoch blocks at height 6 should differ (different forks)"
    );

    // Checkpoint: verify genesis has 7 assignments (5 original + pledge2 + pledge3)
    let genesis_fork_assignments = genesis_node.get_partition_assignments(genesis_addr);
    assert_eq!(
        genesis_fork_assignments.len(),
        7,
        "Genesis should have 7 assignments after 2 fork epochs (5 + pledge2 + pledge3)"
    );

    // Identify the fork-only partition hashes (the 2 new ones not in initial set)
    let genesis_fork_only_hashes: Vec<_> = genesis_fork_assignments
        .iter()
        .filter(|pa| !genesis_initial_partition_hashes.contains(&pa.partition_hash))
        .map(|pa| pa.partition_hash)
        .collect();
    assert_eq!(
        genesis_fork_only_hashes.len(),
        2,
        "Should find 2 fork-only partition hashes for genesis_pledge2 and genesis_pledge3"
    );

    // Verify peer has 3 assignments on its own fork
    let peer_fork_assignments = peer_node.get_partition_assignments(peer_signer.address());
    assert_eq!(
        peer_fork_assignments.len(),
        3,
        "Peer should have 3 assignments after 2 fork epochs (1 + pledge2 + pledge3)"
    );

    // ─── Phase 3: Extend peer's fork to be longer ───

    let (peer_fork_7, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(7, seconds_to_wait).await?;
    error!(
        "Peer mined block 7b at height {}: {}",
        peer_fork_7.height, peer_fork_7.block_hash
    );

    // ─── Phase 4: Trigger reorg ───

    genesis_node.gossip_enable();
    peer_node.gossip_enable();
    IrysNodeTest::announce_between(&genesis_node, &peer_node).await?;
    error!("Gossip re-enabled, gossiping peer's fork to trigger reorg");

    // Gossip peer's fork blocks to genesis
    peer_node.gossip_block_to_peers(&peer_fork_3)?;
    genesis_node
        .wait_for_block(&peer_fork_3.block_hash, seconds_to_wait)
        .await?;

    peer_node.gossip_block_to_peers(&peer_fork_4)?;
    genesis_node
        .wait_for_block(&peer_fork_4.block_hash, seconds_to_wait)
        .await?;

    peer_node.gossip_block_to_peers(&peer_fork_5)?;
    genesis_node
        .wait_for_block(&peer_fork_5.block_hash, seconds_to_wait)
        .await?;

    peer_node.gossip_block_to_peers(&peer_fork_6)?;
    genesis_node
        .wait_for_block(&peer_fork_6.block_hash, seconds_to_wait)
        .await?;

    peer_node.gossip_block_to_peers(&peer_fork_7)?;
    genesis_node
        .wait_for_block(&peer_fork_7.block_hash, seconds_to_wait)
        .await?;

    // Poll until peer's block is canonical at height 7
    {
        let start = Instant::now();
        let timeout = Duration::from_secs(seconds_to_wait as u64);
        loop {
            if let Ok(block) = genesis_node.get_block_by_height(7).await
                && block.block_hash == peer_fork_7.block_hash
            {
                break;
            }
            eyre::ensure!(
                start.elapsed() < timeout,
                "Timeout waiting for peer's block {} to become canonical at height 7",
                peer_fork_7.block_hash
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    error!("Reorg complete: peer's fork is now canonical");

    // ─── Phase 5: Verify multi-epoch reorg state ───

    // Poll until epoch commitment state reflects both epoch transitions
    {
        let start = Instant::now();
        let timeout = Duration::from_secs(seconds_to_wait as u64);

        loop {
            let epoch_snapshot = genesis_node
                .node_ctx
                .block_tree_guard
                .read()
                .canonical_epoch_snapshot();
            let cs = &epoch_snapshot.commitment_state;

            // A. Genesis fork pledges should be absent
            let genesis_has_pledge2 = cs
                .pledge_commitments
                .get(&genesis_addr)
                .map(|v| v.iter().any(|e| e.id == genesis_pledge2.id()))
                .unwrap_or(false);

            let genesis_has_pledge3 = cs
                .pledge_commitments
                .get(&genesis_addr)
                .map(|v| v.iter().any(|e| e.id == genesis_pledge3.id()))
                .unwrap_or(false);

            // B. Peer fork pledges should be present
            let peer_has_pledge2 = cs
                .pledge_commitments
                .get(&peer_signer.address())
                .map(|v| v.iter().any(|e| e.id == peer_pledge2.id()))
                .unwrap_or(false);

            let peer_has_pledge3 = cs
                .pledge_commitments
                .get(&peer_signer.address())
                .map(|v| v.iter().any(|e| e.id == peer_pledge3.id()))
                .unwrap_or(false);

            if !genesis_has_pledge2 && !genesis_has_pledge3 && peer_has_pledge2 && peer_has_pledge3
            {
                info!("Epoch commitment state reflects multi-epoch reorg correctly");
                break;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for epoch commitment state after multi-epoch reorg: \
                     genesis_has_pledge2={genesis_has_pledge2}, \
                     genesis_has_pledge3={genesis_has_pledge3}, \
                     peer_has_pledge2={peer_has_pledge2}, \
                     peer_has_pledge3={peer_has_pledge3}"
                );
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // C. Verify epoch_height reflects the FINAL epoch on the winning fork
    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    assert_eq!(
        epoch_snapshot.epoch_height, 6,
        "Epoch snapshot should be from height 6 (final epoch on winning fork)"
    );

    // D. Partition assignment counts
    let genesis_post_reorg = genesis_node.get_partition_assignments(genesis_addr);
    let peer_post_reorg = genesis_node.get_partition_assignments(peer_signer.address());
    debug!(
        genesis_count = genesis_post_reorg.len(),
        peer_count = peer_post_reorg.len(),
        "Post-reorg partition assignments"
    );
    assert_eq!(
        genesis_post_reorg.len(),
        5,
        "Genesis should revert to 5 assignments (both fork-only pledges reverted)"
    );
    assert_eq!(
        peer_post_reorg.len(),
        3,
        "Peer should have 3 assignments (original + pledge2 + pledge3)"
    );

    // E. Fork-only partition hashes should not appear in genesis assignments
    for hash in &genesis_fork_only_hashes {
        assert!(
            !genesis_post_reorg
                .iter()
                .any(|pa| pa.partition_hash == *hash),
            "Fork-only partition hash {hash} should not be in genesis assignments after reorg"
        );
    }

    // F. Both peer pledges should have partition_hashes assigned
    let cs = &epoch_snapshot.commitment_state;
    let peer_pledge_entries = cs
        .pledge_commitments
        .get(&peer_signer.address())
        .expect("Peer should have pledge entries");
    for pledge_id in [peer_pledge2.id(), peer_pledge3.id()] {
        let entry = peer_pledge_entries
            .iter()
            .find(|e| e.id == pledge_id)
            .unwrap_or_else(|| panic!("Peer pledge {pledge_id} should be in commitment state"));
        assert!(
            entry.partition_hash.is_some(),
            "Peer pledge {pledge_id} should have a partition_hash assigned"
        );
    }

    // G. No genesis storage module assigned to reverted partition hashes
    {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        for sm in sms.iter() {
            if let Some(pa) = sm.partition_assignment() {
                assert!(
                    !genesis_fork_only_hashes.contains(&pa.partition_hash),
                    "No storage module should be assigned to reverted fork-only partitions"
                );
            }
        }
    }

    // ─── Phase 6: Self-correction — mine through next epoch ───

    genesis_node.mine_block().await?;
    peer_node.wait_until_height(8, seconds_to_wait).await?;
    info!("Both nodes synced to height 8 (epoch 4 boundary)");

    genesis_node.wait_for_packing(seconds_to_wait).await;

    // Verify assignments are still consistent after the next epoch
    let genesis_after_epoch4 = genesis_node.get_partition_assignments(genesis_addr);
    let peer_after_epoch4 = genesis_node.get_partition_assignments(peer_signer.address());
    assert_eq!(
        genesis_after_epoch4.len(),
        5,
        "Genesis should still have 5 assignments after epoch 4"
    );
    assert_eq!(
        peer_after_epoch4.len(),
        3,
        "Peer should still have 3 assignments after epoch 4"
    );

    genesis_node.mine_block().await?;
    peer_node.wait_until_height(9, seconds_to_wait).await?;
    info!("Both nodes synced to height 9 — system continues mining after multi-epoch reorg");

    // ─── Phase 7: Cleanup ───

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
