use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error};

#[test_log::test(tokio::test)]
async fn heavy4_fork_recovery_epoch_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // std::env::set_var(
    //     "RUST_LOG",
    //     "debug,irys_actors::block_validation=none;irys_p2p::server=none;irys_actors::mining=error",
    // );
    // initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 10;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for the peer and fund it
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis_node.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testing_peer_with_signer(&peer2_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;

    // Post stake + pledge commitments to peer1
    let peer1_stake_tx = peer1_node.post_stake_commitment(None).await?; // zero() is the genesis block hash
    let peer1_pledge_tx = peer1_node.post_pledge_commitment(None).await?;

    // Post stake + pledge commitments to peer2
    let peer2_stake_tx = peer2_node.post_stake_commitment(None).await?;
    let peer2_pledge_tx = peer2_node.post_pledge_commitment(None).await?;

    // Wait for all commitment tx to show up in the genesis_node's mempool
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

    // Mine a block to get the commitments included
    genesis_node.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    let _ = genesis_node.mine_block().await.unwrap();

    // Get the genesis nodes view of the peers assignments
    let peer1_assignments = genesis_node.get_partition_assignments(peer1_signer.address());
    let peer2_assignments = genesis_node.get_partition_assignments(peer2_signer.address());

    // Verify that one partition has been assigned to each peer to match its pledge
    assert_eq!(peer1_assignments.len(), 1);
    assert_eq!(peer2_assignments.len(), 1);

    // Wait for the peers to receive & process the epoch block
    let _block_hash = peer1_node.wait_until_height(2, seconds_to_wait).await?;
    let _block_hash = peer2_node.wait_until_height(2, seconds_to_wait).await?;

    // Wait for them to pack their storage modules with the partition_hashes
    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    // Disable gossip reception on BOTH peers to prevent cross-contamination
    error!("Disabling gossip reception on both peers to maintain independent forks");
    peer1_node
        .node_ctx
        .sync_state
        .set_gossip_reception_enabled(false);
    peer2_node
        .node_ctx
        .sync_state
        .set_gossip_reception_enabled(false);

    // Now it's time to create different epoch timelines for each peers fork
    let pledge1 = peer1_node
        .post_pledge_commitment_without_gossip(Some(peer1_node.get_anchor().await?))
        .await?;
    let fork1_3 = peer1_node.mine_block().await?;
    error!(
        "Mined fork1_3 at height {}: {}",
        fork1_3.height, fork1_3.block_hash
    );

    let pledge2 = peer2_node
        .post_pledge_commitment_without_gossip(Some(peer2_node.get_anchor().await?))
        .await?;
    let fork2_3 = peer2_node.mine_block().await?;
    error!(
        "Mined fork2_3 at height {}: {}",
        fork2_3.height, fork2_3.block_hash
    );

    // Make sure the blocks on each for have different hashes
    assert_ne!(fork1_3.block_hash, fork2_3.block_hash);
    debug!(
        "Comparing fork heights: fork1 height: {}, fork2 height: {}",
        fork1_3.height, fork2_3.height
    );

    // Have peer1 mine its epoch block first
    let peer1_epoch = peer1_node.mine_block().await?;
    error!(
        "Mined peer1_epoch at height {}: {}",
        peer1_epoch.height, peer1_epoch.block_hash
    );

    // Re-enable gossip reception on peer1 and send its blocks to genesis
    error!("Re-enabling gossip reception on peer1 and gossiping its fork");
    peer1_node
        .node_ctx
        .sync_state
        .set_gossip_reception_enabled(true);

    // Gossip peer1's blocks to genesis (peer2 won't receive due to disabled reception)
    error!("Gossiping fork1_3 to network");
    peer1_node.gossip_block_to_peers(&Arc::new(fork1_3.clone()))?;
    error!("Gossiping peer1_epoch to network");
    peer1_node.gossip_block_to_peers(&Arc::new(peer1_epoch.clone()))?;

    // Wait for genesis to receive peer1's blocks
    let genesis_hash = genesis_node
        .wait_until_height(fork1_3.height, seconds_to_wait)
        .await?;
    assert_eq!(genesis_hash, fork1_3.block_hash);

    // Wait for genesis to receive and accept peer1's epoch block
    // Note: epoch block should be at height 4 (every 2 blocks with num_blocks_in_epoch=2)
    let expected_epoch_height = if peer1_epoch.height == 4 {
        4
    } else {
        peer1_epoch.height
    };
    error!(
        "Waiting for genesis to reach height {} for epoch block",
        expected_epoch_height
    );
    let genesis_epoch_hash = genesis_node
        .wait_until_height(expected_epoch_height, seconds_to_wait)
        .await?;
    assert_eq!(
        peer1_epoch.block_hash, genesis_epoch_hash,
        "Genesis should have peer1's epoch block at height {}",
        expected_epoch_height
    );

    // NOW peer2 can mine its epoch block on its own fork
    // Since gossip reception is disabled, peer2 builds on its own chain
    let peer2_epoch = peer2_node.mine_block().await?;
    error!(
        "Mined peer2_epoch at height {}: {}",
        peer2_epoch.height, peer2_epoch.block_hash
    );

    // Verify the blocks have different parents (confirming they're on different forks)
    assert_ne!(
        peer1_epoch.previous_block_hash, peer2_epoch.previous_block_hash,
        "Peer1 and Peer2 should be on different forks!"
    );

    // TODO: Verify pledge1 is in the genesis epoch state
    // Verify that packing is started on peer1 for whatever partition_hash was assigned to pledge1
    // Verify that a Miner is started on peer1 for whatever partition_hash was assigned to pledge1

    // Then extend peer2's chain to be the longest
    // Peer2 continues on its own fork since gossip reception is still disabled
    let peer2_5 = peer2_node.mine_block().await?;
    let peer2_hash = peer2_5.block_hash;
    let peer2_head = peer2_5.clone();
    error!(
        "Mined peer2_5 at height {}: {}",
        peer2_5.height, peer2_5.block_hash
    );

    // Verify the height 4 block on peer2's chain is not the height 4 on genesis
    assert_ne!(
        peer2_head.previous_block_hash, genesis_epoch_hash,
        "Peer2 should be extending its own fork, not peer1's!"
    );

    // Also verify peer2's block 5 is built on peer2's epoch block
    assert_eq!(
        peer2_head.previous_block_hash, peer2_epoch.block_hash,
        "Peer2's block 5 should be built on peer2's epoch block"
    );

    // Re-enable gossip reception on peer2 so it can send blocks to genesis
    error!("Re-enabling gossip reception on peer2");
    peer2_node
        .node_ctx
        .sync_state
        .set_gossip_reception_enabled(true);

    // Give a moment for network to stabilize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send peer2's fork to genesis to trigger reorg
    // Since we used regular mining, blocks should be in provider and can be sent
    error!("Gossiping peer2's fork to trigger reorg");
    error!("Gossiping fork2_3 (height {})", fork2_3.height);
    peer2_node.gossip_block_to_peers(&Arc::new(fork2_3.clone()))?;

    error!("Gossiping peer2_epoch (height {})", peer2_epoch.height);
    peer2_node.gossip_block_to_peers(&Arc::new(peer2_epoch.clone()))?;

    error!("Gossiping peer2_head (height {})", peer2_head.height);
    peer2_node.gossip_block_to_peers(&Arc::new(peer2_head.clone()))?;

    error!("All blocks gossiped, waiting for reorg");

    // Give genesis time to process the reorg
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Check what genesis has at height 5
    let genesis_hash = genesis_node.wait_until_height(5, seconds_to_wait).await?;
    let genesis_head = genesis_node.get_block_by_hash(&genesis_hash)?;

    // Detailed diagnostics
    if genesis_hash != peer2_hash {
        eprintln!("Reorg did not happen as expected!");
        eprintln!("Genesis has block at height 5: {}", genesis_hash);
        eprintln!("  Parent: {}", genesis_head.previous_block_hash);
        eprintln!("  Cumulative diff: {:?}", genesis_head.cumulative_diff);
        eprintln!("Expected peer2's block: {}", peer2_hash);
        eprintln!("  Parent: {}", peer2_head.previous_block_hash);
        eprintln!("  Cumulative diff: {:?}", peer2_head.cumulative_diff);

        // Check if it's peer1's block
        let peer1_height = peer1_node.get_canonical_chain_height().await;
        if peer1_height >= 5 {
            let peer1_5 = peer1_node.get_block_by_height(5).await?;
            eprintln!("Peer1 also has block at height 5: {}", peer1_5.block_hash);
            if genesis_hash == peer1_5.block_hash {
                panic!("Genesis has peer1's block 5 instead of peer2's!");
            }
        }

        // Check if peer2's chain is actually heavier
        if peer2_head.cumulative_diff <= genesis_head.cumulative_diff {
            panic!(
                "Peer2's chain is not heavier! P2: {:?}, Genesis: {:?}",
                peer2_head.cumulative_diff, genesis_head.cumulative_diff
            );
        }
    }

    assert_eq!(
        genesis_hash, peer2_hash,
        "Genesis should have reorganized to peer2's chain"
    );

    // Wait until the epoch commitment state reflects the reorg effects:
    // - peer2's pledge is present
    // - peer1's pledge is absent
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

            let peer2_has_pledge = cs
                .pledge_commitments
                .get(&peer2_signer.address())
                .map(|v| v.iter().any(|cse| cse.id == pledge2.id()))
                .unwrap_or(false);

            let peer1_has_pledge = cs
                .pledge_commitments
                .get(&peer1_signer.address())
                .map(|v| v.iter().any(|cse| cse.id == pledge1.id()))
                .unwrap_or(false);

            if peer2_has_pledge && !peer1_has_pledge {
                break;
            }

            if start.elapsed() > timeout {
                panic!(
                    "Timeout waiting for epoch commitment state after reorg: peer2_has_pledge={}, peer1_has_pledge={}",
                    peer2_has_pledge, peer1_has_pledge
                );
            }

            // Sleep briefly and retry
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // TODO:
        // Verify that packing is started on peer2 for whatever partition_hash was assigned to pledge2
        // Verify that a Miner is started on peer2 for whatever partition_hash was assigned to pledge2
        // Verify that packing is stopped on peer1 for whatever partition_hash was assigned to pledge1
        // Verify that a Miner is stopped on peer1 for whatever partition_hash was assigned to pledge1
        //
    }

    // Wind down test
    genesis_node.stop().await;
    peer1_node.stop().await;
    peer2_node.stop().await;
    Ok(())
}
