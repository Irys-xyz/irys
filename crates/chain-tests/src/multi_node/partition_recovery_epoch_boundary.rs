use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_domain::ChunkType;
use irys_types::NodeConfig;
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

/// Tests that partition assignments are correctly rolled back when a reorg crosses
/// an epoch boundary where each fork processed different pledge commitments.
///
/// Timeline:
///   Blocks 1–2  (gossip ON)  — shared chain, peer stakes+pledges, epoch boundary
///   Blocks 3–4  (gossip OFF) — each node posts unique pledge, mines through epoch
///   Block 5     (gossip OFF) — peer extends fork to be longer
///   Gossip ON   — peer's fork gossiped to genesis, triggers reorg
///
/// Verifies on genesis after reorg:
///   - Genesis's fork-only pledge is absent from commitment state
///   - Peer's fork-only pledge is present in commitment state
///   - Genesis partition assignments revert (6 → 5)
///   - Peer partition assignments advance (1 → 2)
///   - The fork-only partition hash is not assigned to any storage module
///   - The cleared storage module gets re-packed after reorg
#[test_log::test(tokio::test)]
async fn heavy4_partition_recovery_epoch_boundary() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 10;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // ─── Phase 0: Start nodes ───

    // Use 5 submodules for genesis so the 4th pledge during the fork actually gets
    // a physical storage module. Default 3 wouldn't leave room for the fork-only pledge.
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

    // Block 2: epoch boundary → assigns partition hashes
    genesis_node.mine_block().await?;
    peer_node.wait_until_height(2, seconds_to_wait).await?;
    info!("Both nodes synced to height 2 (epoch boundary)");

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

    // ─── Phase 2: Fork — each node posts additional pledge ───

    genesis_node.gossip_disable();
    peer_node.gossip_disable();
    error!("Gossip disabled, fork point at height 2");

    // Genesis posts a 6th pledge (minority fork only, 5 auto-pledges already exist)
    let genesis_pledge2 = genesis_node
        .post_pledge_commitment_without_gossip(Some(genesis_node.get_anchor().await?))
        .await?;
    info!(id = %genesis_pledge2.id(), "Genesis posted fork-only pledge");

    // Peer posts a 2nd pledge (majority fork only)
    let peer_pledge2 = peer_node
        .post_pledge_commitment_without_gossip(Some(peer_node.get_anchor().await?))
        .await?;
    info!(id = %peer_pledge2.id(), "Peer posted fork-only pledge");

    // Genesis mines block 3a (includes genesis_pledge2)
    let (genesis_fork_3, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(3, seconds_to_wait).await?;
    error!(
        "Genesis mined fork block 3a at height {}: {}",
        genesis_fork_3.height, genesis_fork_3.block_hash
    );

    // Peer mines block 3b (includes peer_pledge2)
    let (peer_fork_3, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(3, seconds_to_wait).await?;
    error!(
        "Peer mined fork block 3b at height {}: {}",
        peer_fork_3.height, peer_fork_3.block_hash
    );

    assert_ne!(
        genesis_fork_3.block_hash, peer_fork_3.block_hash,
        "Fork blocks at height 3 should differ"
    );

    // Genesis mines block 4a (EPOCH BOUNDARY on fork A)
    let (genesis_fork_4, _, _) = genesis_node.mine_block_without_gossip().await?;
    genesis_node.wait_until_height(4, seconds_to_wait).await?;
    error!(
        "Genesis mined fork epoch block 4a at height {}: {}",
        genesis_fork_4.height, genesis_fork_4.block_hash
    );

    // Peer mines block 4b (EPOCH BOUNDARY on fork B)
    let (peer_fork_4, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(4, seconds_to_wait).await?;
    error!(
        "Peer mined fork epoch block 4b at height {}: {}",
        peer_fork_4.height, peer_fork_4.block_hash
    );

    assert_ne!(
        genesis_fork_4.block_hash, peer_fork_4.block_hash,
        "Epoch blocks at height 4 should differ (different forks)"
    );

    // Checkpoint: verify genesis now has 6 assignments (5 original + genesis_pledge2)
    let genesis_fork_assignments = genesis_node.get_partition_assignments(genesis_addr);
    assert_eq!(
        genesis_fork_assignments.len(),
        6,
        "Genesis should have 6 assignments after fork epoch (5 + genesis_pledge2)"
    );

    // Identify the fork-only partition hash (the one not in initial set)
    let genesis_pledge2_partition_hash = genesis_fork_assignments
        .iter()
        .find(|pa| !genesis_initial_partition_hashes.contains(&pa.partition_hash))
        .expect("Should find the new partition assigned to genesis_pledge2")
        .partition_hash;

    // Capture which physical storage module holds this fork-only partition.
    // We'll track this module through the reorg to verify it gets cleared and reassigned.
    let affected_module_id = {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .find(|sm| {
                sm.partition_assignment()
                    .is_some_and(|pa| pa.partition_hash == genesis_pledge2_partition_hash)
            })
            .map(|sm| sm.id)
    };
    // With 5 submodules and 6 assignments, only 5 get physical modules. The 6th may not.
    // If it did get a module, we'll do targeted verification after the reorg.
    info!(
        hash = %genesis_pledge2_partition_hash,
        module_id = ?affected_module_id,
        "Identified genesis fork-only partition hash"
    );

    // Verify peer has 2 assignments on its own fork
    let peer_fork_assignments = peer_node.get_partition_assignments(peer_signer.address());
    assert_eq!(
        peer_fork_assignments.len(),
        2,
        "Peer should have 2 assignments after fork epoch (1 + peer_pledge2)"
    );

    // ─── Phase 3: Extend peer's fork to be longer ───

    let (peer_fork_5, _, _) = peer_node.mine_block_without_gossip().await?;
    peer_node.wait_until_height(5, seconds_to_wait).await?;
    error!(
        "Peer mined block 5b at height {}: {}",
        peer_fork_5.height, peer_fork_5.block_hash
    );

    // ─── Phase 4: Trigger reorg ───

    // Re-enable gossip and re-announce so peers are marked online
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

    // Poll until peer's block is canonical at height 5
    {
        let start = Instant::now();
        let timeout = Duration::from_secs(seconds_to_wait as u64);
        loop {
            if let Ok(block) = genesis_node.get_block_by_height(5).await
                && block.block_hash == peer_fork_5.block_hash
            {
                break;
            }
            eyre::ensure!(
                start.elapsed() < timeout,
                "Timeout waiting for peer's block {} to become canonical at height 5",
                peer_fork_5.block_hash
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    error!("Reorg complete: peer's fork is now canonical");

    // ─── Phase 5: Verify partition assignment rollback ───

    // Poll until epoch commitment state reflects the reorg
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

            // A. Commitment state: genesis_pledge2 should be absent
            let genesis_has_fork_pledge = cs
                .pledge_commitments
                .get(&genesis_addr)
                .map(|v| v.iter().any(|e| e.id == genesis_pledge2.id()))
                .unwrap_or(false);

            // A. Commitment state: peer_pledge2 should be present
            let peer_has_new_pledge = cs
                .pledge_commitments
                .get(&peer_signer.address())
                .map(|v| v.iter().any(|e| e.id == peer_pledge2.id()))
                .unwrap_or(false);

            if !genesis_has_fork_pledge && peer_has_new_pledge {
                info!("Epoch commitment state reflects reorg correctly");
                break;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for epoch commitment state after reorg: \
                     genesis_has_fork_pledge={genesis_has_fork_pledge}, \
                     peer_has_new_pledge={peer_has_new_pledge}"
                );
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // B. Partition assignment counts
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
        "Genesis should revert to 5 assignments (fork-only pledge reverted)"
    );
    assert_eq!(
        peer_post_reorg.len(),
        2,
        "Peer should have 2 assignments (original + peer_pledge2)"
    );

    // C. The fork-only partition hash should not appear in genesis assignments
    assert!(
        !genesis_post_reorg
            .iter()
            .any(|pa| pa.partition_hash == genesis_pledge2_partition_hash),
        "Fork-only partition hash should not be in genesis assignments after reorg"
    );

    // C. Peer's pledge2 should have a partition_hash assigned
    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let peer_pledge2_entries = epoch_snapshot
        .commitment_state
        .pledge_commitments
        .get(&peer_signer.address())
        .expect("Peer should have pledge entries");
    let peer_pledge2_entry = peer_pledge2_entries
        .iter()
        .find(|e| e.id == peer_pledge2.id())
        .expect("Peer pledge2 should be in commitment state");
    assert!(
        peer_pledge2_entry.partition_hash.is_some(),
        "Peer pledge2 should have a partition_hash assigned"
    );

    // D. Storage module verification: no genesis storage module assigned to reverted partition
    {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        for sm in sms.iter() {
            if let Some(pa) = sm.partition_assignment() {
                assert_ne!(
                    pa.partition_hash, genesis_pledge2_partition_hash,
                    "No storage module should be assigned to the reverted fork-only partition"
                );
            }
        }
    }

    // E. Wait for packing to complete after the reorg — the cleared storage module
    //    should be reassigned to a canonical partition and re-packed.
    genesis_node.wait_for_packing(seconds_to_wait).await;

    // Verify every genesis storage module has an assignment after re-packing.
    // With 5 submodules and 5 canonical assignments, all should be mapped.
    {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        let assigned_count = sms
            .iter()
            .filter(|sm| sm.partition_assignment().is_some())
            .count();
        debug!(
            assigned_count,
            total = sms.len(),
            "Storage modules after re-packing"
        );
        assert_eq!(
            assigned_count,
            sms.len(),
            "All storage modules should have partition assignments after re-packing"
        );
    }

    // F. Targeted verification: if the fork-only partition had a physical module,
    //    verify that specific module was reassigned to a different canonical partition
    //    and has Entropy intervals from re-packing.
    if let Some(module_id) = affected_module_id {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        let affected_module = sms
            .iter()
            .find(|sm| sm.id == module_id)
            .expect("Affected module should still exist");

        let new_assignment = affected_module
            .partition_assignment()
            .expect("Affected module should have a new assignment after re-packing");

        assert_ne!(
            new_assignment.partition_hash, genesis_pledge2_partition_hash,
            "Affected module (id={module_id}) should have been reassigned \
             to a canonical partition, not the reverted fork-only one"
        );
        assert!(
            genesis_initial_partition_hashes.contains(&new_assignment.partition_hash),
            "Affected module (id={module_id}) should be assigned to one of the \
             original canonical partitions after reorg"
        );

        // Verify the module has Entropy intervals, proving the packing service
        // actually processed the re-pack request (not just that an assignment exists).
        let entropy_intervals = affected_module.get_intervals(ChunkType::Entropy);
        assert!(
            !entropy_intervals.is_empty(),
            "Affected module (id={module_id}) should have Entropy intervals \
             after re-packing, proving the PackingRequest was executed"
        );
        info!(
            module_id,
            new_hash = %new_assignment.partition_hash,
            entropy_intervals = entropy_intervals.len(),
            "Affected module successfully cleared, reassigned, and re-packed"
        );
    }
    info!("All partition assignment rollback and re-packing verifications passed");

    // ─── Phase 6: Cleanup ───

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
