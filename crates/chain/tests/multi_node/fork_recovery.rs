use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_types::{DataLedger, DataTransaction, NodeConfig, UnixTimestamp, H256, U256};
use reth::rpc::types::BlockNumberOrTag;
use std::sync::Arc;
use tracing::debug;

#[test_log::test(tokio::test)]
async fn slow_heavy3_fork_recovery_submit_tx_test() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 15;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

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
    let block1 = genesis_node.mine_block().await.unwrap();

    debug!("block1: {}", block1.height);

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    let block2 = genesis_node.mine_block().await.unwrap();

    debug!("block1: {} block2: {}", block1.height, block2.height);

    // wait for block mining to reach tree height
    genesis_node
        .wait_for_block_at_height(2, seconds_to_wait)
        .await?;

    // wait for migration to reach index height
    genesis_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    // Get the genesis nodes view of the peers assignments
    let peer1_assignments = genesis_node.get_partition_assignments(peer1_signer.address());
    let peer2_assignments = genesis_node.get_partition_assignments(peer2_signer.address());

    // Verify that one partition has been assigned to each peer to match its pledge
    assert_eq!(peer1_assignments.len(), 1);
    assert_eq!(peer2_assignments.len(), 1);

    // Wait for the peers to receive & process the epoch block
    peer1_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    // Wait for them to pack their storage modules with the partition_hashes
    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    let chunks1 = [[10; 32], [20; 32], [30; 32]];
    let data1: Vec<u8> = chunks1.concat();

    let chunks2 = [[40; 32], [50; 32], [60; 32]];
    let data2: Vec<u8> = chunks2.concat();

    let chunks3 = [[70; 32], [80; 32], [90; 32]];
    let data3: Vec<u8> = chunks3.concat();

    // Post a transaction that should be gossiped to all peers
    let shared_tx = genesis_node
        .post_data_tx(
            genesis_node.get_anchor().await?,
            data3,
            &genesis_node.node_ctx.config.irys_signer(),
        )
        .await;

    // Wait for the transaction to gossip
    let txid = shared_tx.header.id;
    peer1_node.wait_for_mempool(txid, seconds_to_wait).await?;
    peer2_node.wait_for_mempool(txid, seconds_to_wait).await?;

    // Post a unique storage transaction to each peer
    let peer1_tx = peer1_node
        .post_data_tx_without_gossip(peer1_node.get_anchor().await?, data1, &peer1_signer)
        .await;
    let peer2_tx = peer2_node
        .post_data_tx_without_gossip(peer2_node.get_anchor().await?, data2, &peer2_signer)
        .await;

    // Mine mine blocks on both peers in parallel
    let (result1, result2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1)
    );

    // Fail the test on any error results
    result1?;
    result2?;

    // wait for block mining to reach tree height
    peer1_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    peer2_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    // wait for migration to reach index height
    peer1_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await?;

    // Validate the peer blocks create forks with different transactions
    let peer1_block = peer1_node.get_block_by_height(3).await?;
    let peer2_block = peer2_node.get_block_by_height(3).await?;

    let peer1_block_txids = &peer1_block.data_ledgers[DataLedger::Submit].tx_ids.0;
    assert!(peer1_block_txids.contains(&txid));
    assert!(peer1_block_txids.contains(&peer1_tx.header.id));

    let peer2_block_txids = &peer2_block.data_ledgers[DataLedger::Submit].tx_ids.0;
    assert!(peer2_block_txids.contains(&txid));
    assert!(peer2_block_txids.contains(&peer2_tx.header.id));

    // Assert both blocks have the same cumulative difficulty this will ensure
    // that the peers prefer the first block they saw with this cumulative difficulty,
    // their own.
    assert_eq!(peer1_block.cumulative_diff, peer2_block.cumulative_diff);

    let peer2_block = Arc::new(peer2_block);
    let peer1_block = Arc::new(peer1_block);

    peer2_node.gossip_block_to_peers(&peer2_block)?;
    peer1_node.gossip_block_to_peers(&peer1_block)?;

    // Wait for gossip, to send blocks to opposite peers
    peer1_node
        .wait_for_block(&peer2_block.block_hash, 10)
        .await?;
    peer2_node
        .wait_for_block(&peer1_block.block_hash, 10)
        .await?;
    peer1_node.get_block_by_hash(&peer2_block.block_hash)?;
    peer2_node.get_block_by_hash(&peer1_block.block_hash)?;

    let peer1_block_after = peer1_node.get_block_by_height(3).await?;
    let peer2_block_after = peer2_node.get_block_by_height(3).await?;

    // Verify neither peer changed their blocks after receiving the other peers block
    // for the same height.
    assert_eq!(peer1_block_after.block_hash, peer1_block.block_hash);
    assert_eq!(peer2_block_after.block_hash, peer2_block.block_hash);

    // wait for genesis block tree height 3
    genesis_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    let genesis_block = genesis_node.get_block_by_height(3).await?;
    //wait for genesis block index height 2
    // FIXME: genesis_node.wait_until_height_on_chain(2) sometimes fails
    genesis_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await
        .expect("expected genesis to index block 2");

    debug!(
        "\nPEER1\n    before: {} c_diff: {}\n    after:  {} c_diff: {}\nPEER2\n    before: {} c_diff: {}\n    after:  {} c_diff: {}",
        peer1_block.block_hash,
        peer1_block.cumulative_diff,
        peer1_block_after.block_hash,
        peer1_block_after.cumulative_diff,
        peer2_block.block_hash,
        peer2_block.cumulative_diff,
        peer2_block_after.block_hash,
        peer2_block_after.cumulative_diff,
    );
    debug!(
        "\nGENESIS: {:?} height: {}",
        genesis_block.block_hash, genesis_block.height
    );

    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);

    let canon_before = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain();

    // Determine which peer lost the fork race and extend the other peer's chain
    // to trigger a reorganization. The losing peer's transaction will be evicted
    // and returned to the mempool.
    let reorg_tx: DataTransaction;
    let _reorg_block_hash: H256;
    let _reorg_block = if genesis_block.block_hash == peer1_block.block_hash {
        debug!(
            "GENESIS: should ignore {} and should already be on {} height: {}",
            peer2_block.block_hash, peer1_block.block_hash, genesis_block.height
        );
        _reorg_block_hash = peer1_block.block_hash;
        reorg_tx = peer1_tx; // Peer1 won initially, so peer2's chain will overtake it
        peer2_node.mine_block().await?;
        peer2_node.get_block_by_height(4).await?
    } else {
        debug!(
            "GENESIS: should ignore {} and should already be on {} height: {}",
            peer1_block.block_hash, peer2_block.block_hash, genesis_block.height
        );
        _reorg_block_hash = peer2_block.block_hash;
        reorg_tx = peer2_tx; // Peer2 won initially, so peer1's chain will overtake it
        peer1_node.mine_block().await?;
        peer1_node.get_block_by_height(4).await?
    };

    let reorg_event = reorg_future.await?;
    let _genesis_block = genesis_node.get_block_by_height(4).await?;

    debug!("{:?}", reorg_event);
    let canon = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain();

    let old_fork_hashes: Vec<_> = reorg_event
        .old_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();
    let new_fork_hashes: Vec<_> = reorg_event
        .new_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();

    println!(
        "\nReorgEvent:\n fork_parent: {:?}\n old_fork: {:?}\n new_fork:{:?}",
        reorg_event.fork_parent.block_hash, old_fork_hashes, new_fork_hashes
    );

    println!("\nreorg_tx: {:?}", reorg_tx.header.id);
    println!("canonical_before:");
    for entry in &canon_before.0 {
        println!("  {:?}", entry)
    }
    println!("canonical_after:");
    for entry in &canon.0 {
        println!("  {:?}", entry)
    }

    // assert_eq!(reorg_event.orphaned_blocks, vec![reorg_block_hash]);

    // Make sure the reorg_tx is back in the mempool ready to be included in the next block
    // NOTE: It turns out the reorg_tx is actually in the block because all tx are gossiped
    //       along with their blocks even if they are a fork, so when the peer
    //       extends their fork, they have the fork tx in their mempool already
    //       and it gets included in the block.
    // let pending_tx = genesis_node.get_best_mempool_tx().await;
    // let tx = pending_tx
    //     .storage_tx
    //     .iter()
    //     .find(|tx| tx.id == reorg_tx.header.id);
    // assert_eq!(tx, Some(&reorg_tx.header));

    // Validate the ReorgEvent with the canonical chains
    let old_fork: Vec<_> = reorg_event
        .old_fork
        .iter()
        .map(|bh| bh.header().block_hash)
        .collect();

    let new_fork: Vec<_> = reorg_event
        .new_fork
        .iter()
        .map(|bh| bh.header().block_hash)
        .collect();

    println!("\nfork_parent: {:?}", reorg_event.fork_parent.block_hash);
    println!("old_fork:\n  {:?}", old_fork);
    println!("new_fork:\n  {:?}", new_fork);

    assert_eq!(old_fork, vec![canon_before.0.last().unwrap().block_hash()]);
    assert_eq!(
        new_fork,
        vec![
            canon.0[canon.0.len() - 2].block_hash(),
            canon.0.last().unwrap().block_hash()
        ]
    );

    assert_eq!(reorg_event.new_tip, *new_fork.last().unwrap());

    // Wind down test
    tokio::join!(genesis_node.stop(), peer1_node.stop(), peer2_node.stop());
    Ok(())
}

/// Simulates a shallow fork produced by a single peer while gossip is disabled.
/// The peer begins mining first, the genesis node continues extending the canonical chain,
/// and the fork overtakes the main chain once gossip resumes.
/// Assertions cover:
/// - Block index alignment with reth FCU state.
/// - Reth FCU propagation (`Latest`, `Safe`, `Finalized`) matching the canonical/migrated blocks.
/// - Reth latest tags on both nodes reflect their respective fork tips before the reorg.
#[test_log::test(tokio::test)]
async fn heavy_shallow_fork_triggers_migration_prune_and_fcu() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    let num_blocks_in_epoch = 3;
    let block_tree_depth: u64 = 6;
    let block_migration_depth: u32 = 4;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_tree_depth = block_tree_depth;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = genesis_node
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await?;

    let base_height = genesis_node.get_canonical_chain_height().await;

    // Stage 1: peer mines a private fork with gossip disabled
    peer_node.gossip_disable();

    peer_node.mine_blocks_without_gossip(1).await?;
    let fork_height = base_height + 1;
    let fork_block_level1 = peer_node.get_block_by_height(fork_height).await?;

    // Stage 2: genesis extends the canonical chain while unaware of the fork
    genesis_node.gossip_disable();
    let canonical_block_level1 = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(canonical_block_level1.height, seconds_to_wait)
        .await?;

    let canonical_block_level2 = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(canonical_block_level2.height, seconds_to_wait)
        .await?;

    let _ = genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            canonical_block_level2.evm_block_hash,
            seconds_to_wait as u64,
        )
        .await?;

    // Stage 3: peer privately extends the fork beyond canonical length and ensures its Reth view is updated
    peer_node.mine_blocks_without_gossip(2).await?;
    let fork_block_level2 = peer_node.get_block_by_height(fork_height + 1).await?;
    let fork_block_level3 = peer_node.get_block_by_height(fork_height + 2).await?;
    let canonical_cumulative_diff = canonical_block_level2.cumulative_diff;

    let mut fork_blocks = vec![
        fork_block_level1.clone(),
        fork_block_level2.clone(),
        fork_block_level3.clone(),
    ];
    let mut fork_tip = fork_block_level3.clone();
    let mut extra_private_blocks = 0_usize;

    // Height alone is not enough to force a reorg when per-block difficulty varies.
    // Keep extending the private fork until it is strictly heavier than genesis' canonical tip.
    while fork_tip.cumulative_diff <= canonical_cumulative_diff {
        peer_node.mine_blocks_without_gossip(1).await?;
        let next_height = fork_tip.height + 1;
        fork_tip = peer_node.get_block_by_height(next_height).await?;
        fork_blocks.push(fork_tip.clone());
        extra_private_blocks += 1;
        eyre::ensure!(
            extra_private_blocks <= 6,
            "Private fork failed to exceed canonical cumulative difficulty after {} extra blocks (canonical={}, tip={})",
            extra_private_blocks,
            canonical_cumulative_diff,
            fork_tip.cumulative_diff
        );
    }

    let _ = peer_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            fork_tip.evm_block_hash,
            seconds_to_wait as u64,
        )
        .await?;

    debug!(
        "canonical_diff={} fork_tip_diff={} fork_tip_height={} fork_blocks_to_reveal={}",
        canonical_cumulative_diff,
        fork_tip.cumulative_diff,
        fork_tip.height,
        fork_blocks.len()
    );

    let fork_arc_level1 = Arc::new(fork_blocks[0].clone());
    let fork_reveal_tail: Vec<Arc<_>> = fork_blocks.iter().skip(1).cloned().map(Arc::new).collect();

    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);

    // Stage 4: peer reveals the first fork block (still not longer than canonical)
    peer_node.gossip_enable();
    genesis_node.gossip_enable();
    peer_node.gossip_block_to_peers(&fork_arc_level1)?;
    genesis_node
        .wait_for_block(&fork_block_level1.block_hash, seconds_to_wait)
        .await?;

    let _ = genesis_node
        .wait_for_reth_marker(
            BlockNumberOrTag::Latest,
            canonical_block_level2.evm_block_hash,
            seconds_to_wait as u64,
        )
        .await?;

    // Stage 5: peer gossips additional fork blocks to overtake canonical tip
    for block in &fork_reveal_tail {
        peer_node.gossip_block_to_peers(block)?;
        genesis_node
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }

    let reorg_event = reorg_future.await?;

    let extension_height = fork_tip.height;
    genesis_node
        .wait_for_block_at_height(extension_height, seconds_to_wait)
        .await?;
    peer_node
        .wait_for_block_at_height(extension_height, seconds_to_wait)
        .await?;

    let chain_tip_height = genesis_node.get_canonical_chain_height().await;
    assert_eq!(
        chain_tip_height, extension_height,
        "expected tip to match fork extension height"
    );

    let migration_depth_u64 = block_migration_depth as u64;
    let prune_depth_u64 = block_tree_depth;
    let migration_height = chain_tip_height.saturating_sub(migration_depth_u64);
    let prune_height = chain_tip_height.saturating_sub(prune_depth_u64);

    genesis_node
        .wait_for_block_in_index(migration_height, false, seconds_to_wait)
        .await?;

    let head_block = genesis_node.get_block_by_height(chain_tip_height).await?;
    let migration_block = genesis_node.get_block_by_height(migration_height).await?;
    let prune_block = genesis_node.get_block_by_height_from_index(prune_height, false)?;

    for node in [&genesis_node, &peer_node] {
        // Stage 6: validate block index contains migrated block & pruned block
        {
            let block_index_guard = node.node_ctx.block_index_guard.read();
            let migrated_entry = block_index_guard
                .get_item(migration_height)
                .expect("migration height missing from block index");
            assert_eq!(
                migrated_entry.block_hash, migration_block.block_hash,
                "migration height should match canonical block"
            );

            let latest_entry = block_index_guard
                .get_latest_item()
                .expect("block index should have at least one entry");
            assert_eq!(
                latest_entry.block_hash, migration_block.block_hash,
                "latest block index entry should align with migration block"
            );

            let pruned_entry = block_index_guard
                .get_item(prune_height)
                .expect("prune height missing from block index");
            assert_eq!(
                pruned_entry.block_hash, prune_block.block_hash,
                "prune height should track canonical block"
            );
        }

        // Stage 7: block tree does not contain pruned block
        {
            let block_tree_guard = node.node_ctx.block_tree_guard.read();
            let (canonical_entries, _) = block_tree_guard.get_canonical_chain();
            let last_entry = canonical_entries.first().unwrap();
            let last_entry = block_tree_guard
                .get_block(&last_entry.block_hash())
                .unwrap();
            assert_eq!(
                last_entry.height,
                prune_height + 1,
                "tip height should be exactly one greater than pruned block height"
            );
            assert_eq!(
                last_entry.previous_block_hash, prune_block.block_hash,
                "tip should reference the pruned block as its parent"
            );
            let pruned_block_in_tree = block_tree_guard.get_block(&prune_block.block_hash);
            assert!(
                pruned_block_in_tree.is_none(),
                "block at prune depth should be removed from block tree"
            );
        }

        // reth internal tracking state matches the expected irys heights
        let _ = node
            .wait_for_reth_marker(
                BlockNumberOrTag::Latest,
                head_block.evm_block_hash,
                seconds_to_wait as u64,
            )
            .await?;
        let safe_hash = node
            .wait_for_reth_marker(
                BlockNumberOrTag::Safe,
                migration_block.evm_block_hash,
                seconds_to_wait as u64,
            )
            .await?;
        assert_eq!(
            safe_hash, migration_block.evm_block_hash,
            "reth safe tag should reference migration block"
        );
        let _ = node
            .wait_for_reth_marker(
                BlockNumberOrTag::Finalized,
                prune_block.evm_block_hash,
                seconds_to_wait as u64,
            )
            .await?;
    }

    assert_eq!(reorg_event.new_tip, head_block.block_hash);

    tokio::join!(genesis_node.stop(), peer_node.stop());
    Ok(())
}

/// Reorg where there are 3 forks and the tip moves across all of them as each is extended longer than the other.
///   We need to verify that
///    - commitment txs are eligible for inclusion in future blocks once they are no longer part of the canonical chain
///    - commitment txs do not appear twice, or are missing from canonical chain
///    - all canonical blocks move to all peers
///    - TODO: all the balance changes that were applied in one fork are reverted during the Reorg
///    - TODO: new balance changes are applied based on the new canonical branch
#[test_log::test(tokio::test)]
async fn heavy_reorg_tip_moves_across_nodes_commitment_txs() -> eyre::Result<()> {
    // config variables
    let num_blocks_in_epoch = 5; // test currently mines 4 blocks, and expects txs to remain in mempool
    let seconds_to_wait = 15;

    // setup config
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    // signers
    let b_signer = genesis_config.new_random_signer();
    let c_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&b_signer, &c_signer]);

    // genesis node / node_a
    let node_a = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // additional configs for peers
    let config_b = node_a.testing_peer_with_signer(&c_signer);
    let config_c = node_a.testing_peer_with_signer(&b_signer);

    // start peer nodes
    let node_b = IrysNodeTest::new(config_b)
        .start_and_wait_for_packing("NODE_B", seconds_to_wait)
        .await;
    let node_c = IrysNodeTest::new(config_c)
        .start_and_wait_for_packing("NODE_C", seconds_to_wait)
        .await;

    //
    // Stage 1: STARTING STATE CHECKS
    //

    // check peer heights match genesis - i.e. that we are all in sync
    let current_height = node_a.get_canonical_chain_height().await;
    assert_eq!(current_height, 0);
    node_b
        .wait_until_height(current_height, seconds_to_wait)
        .await?;
    node_c
        .wait_until_height(current_height, seconds_to_wait)
        .await?;

    //
    // Stage 2: MINE BLOCK
    //

    // mine a single block, and let everyone sync so future txs start at block height 1.
    node_a.mine_block().await?; // mine block a1
    node_a.wait_until_height(1, seconds_to_wait).await?;
    let block_height_1 = node_a.get_block_by_height(1).await?; // get block a1
    node_b
        .wait_for_block(&block_height_1.block_hash, seconds_to_wait)
        .await?;
    node_c
        .wait_for_block(&block_height_1.block_hash, seconds_to_wait)
        .await?;

    assert_eq!(
        block_height_1.system_ledgers.len(),
        0,
        "No txs should exist to be included in this block"
    );

    //
    // Stage 3: DISABLE ANY/ALL GOSSIP
    //
    {
        node_a.gossip_disable();
        node_b.gossip_disable();
        node_c.gossip_disable();
    }

    //
    // Stage 4: GENERATE ISOLATED txs
    //

    let anchor = node_a.get_anchor().await?;
    // node_b generates txs in isolation for inclusion in block 2
    let peer_b_b2_stake_tx = node_b
        .post_stake_commitment_without_gossip(Some(anchor))
        .await?;
    let peer_b_b2_pledge_tx = node_b
        .post_pledge_commitment_without_gossip(Some(anchor))
        .await?;

    // node_c generates txs in isolation for inclusion block 2
    let peer_c_b2_stake_tx = node_c
        .post_stake_commitment_without_gossip(Some(anchor))
        .await?;
    let peer_c_b2_pledge_tx = node_c
        .post_pledge_commitment_without_gossip(Some(anchor))
        .await?;

    //
    // Stage 5: MINE FORK A and B TO HEIGHT 2 and 3
    //

    // Mine competing blocks on A and B without gossip
    let (a_block2, _, _a_block2_txs) = node_a.mine_block_without_gossip().await?; // block a2
    node_a
        .wait_until_height(a_block2.height, seconds_to_wait)
        .await?;

    let (b_block2, b_block2_payload, b_block2_txs) = node_b.mine_block_without_gossip().await?; // block b2
    node_b
        .wait_until_height(b_block2.height, seconds_to_wait)
        .await?;
    let (b_block3, b_block3_payload, b_block3_txs) = node_b.mine_block_without_gossip().await?; // block b3
    node_b
        .wait_until_height(b_block3.height, seconds_to_wait)
        .await?;

    // check how many txs made it into each block, we expect no more than 2
    assert_eq!(
        b_block2.system_ledgers.len(),
        1,
        "Expect 1 of the 2 isolated txs on peer B to be in this block. The stake tx and not the pledge tx."
    );
    assert_eq!(
        a_block2.system_ledgers.len(),
        0,
        "No txs should have been gossiped back to peer A"
    ); // 0 commitments, also means 0 system ledgers

    // NODE B -> Node C
    // send full blocks to node c, this includes commitment txs for block 2
    // this will cause a reorg on node c (which is only height 2) to match the chain on node b (height 3)
    // this will cause the txs that were previously canonical from C2 to become non canon
    {
        node_b
            .send_full_block(
                &node_c,
                &b_block2,
                b_block2_payload.clone(),
                b_block2_txs.clone(),
            )
            .await?;
        tracing::error!("posted block 2: {:?}", b_block2.block_hash);
        node_b
            .send_full_block(
                &node_c,
                &b_block3,
                b_block3_payload.clone(),
                b_block3_txs.clone(),
            )
            .await?;
        tracing::error!("posted block 3: {:?}", b_block3.block_hash);

        node_c.wait_for_block(&b_block2.block_hash, 10).await?;
        node_c.wait_for_block(&b_block3.block_hash, 10).await?;
        // check node A has not received blocks from B
        assert!(
            node_a
                .wait_for_block(&b_block2.block_hash, 1)
                .await
                .is_err(),
            "Node A should not yet have received block 2 from Node B"
        );
        assert!(
            node_a
                .wait_for_block(&b_block3.block_hash, 1)
                .await
                .is_err(),
            "Node A should not yet have received block 3 from Node B"
        );
    }

    //
    // Stage 6: MINE FORK C TO HEIGHT 4
    //

    // Node C mines on top of B's chain and does not gossip it back to B
    // Node C has the non canon txs from it's now non canon block 2.
    // Node C will choose to include these txs in block C4
    if let Err(does_not_reach_height) = node_c.wait_until_height(3, seconds_to_wait).await {
        tracing::error!(
            "Node C Failed to reach block height 3: {:?}",
            does_not_reach_height
        );
        Err(does_not_reach_height)?
    }
    let (c_block4, _c_block4_payload, _c_block4_txs) = node_c.mine_block_without_gossip().await?;
    if let Err(does_not_reach_height) = node_c.wait_until_height(4, seconds_to_wait).await {
        tracing::error!(
            "Node C Failed to reach block height 4: {:?}",
            does_not_reach_height
        );
    }
    assert_eq!(c_block4.height, 4, "Node C Failed to reach block height 4"); // block c4

    //
    // Stage 7: FINAL SYNC / RE-ORGs
    //
    {
        // Enable gossip
        node_a.gossip_enable();
        node_b.gossip_enable();
        node_c.gossip_enable();
        // Gossip all blocks so everyone syncs
        node_b.gossip_block_to_peers(&b_block2)?;
        node_b.gossip_block_to_peers(&b_block3)?;
        node_c.gossip_block_to_peers(&c_block4)?;
        node_a.gossip_block_to_peers(&a_block2)?;
    }
    //
    // Stage 8: FINAL STATE CHECKS
    //

    // confirm all three nodes are at the same and expected height "4"
    {
        node_a
            .wait_until_height(c_block4.height, seconds_to_wait)
            .await?;
        node_b
            .wait_until_height(c_block4.height, seconds_to_wait)
            .await?;
        node_c
            .wait_until_height(c_block4.height, seconds_to_wait)
            .await?;

        // confirm chain has identical and expected height on all three nodes
        let a_latest_height = node_a.get_canonical_chain_height().await;
        let b_latest_height = node_b.get_canonical_chain_height().await;
        let c_latest_height = node_c.get_canonical_chain_height().await;
        assert_eq!(a_latest_height, c_block4.height);
        assert_eq!(a_latest_height, b_latest_height);
        assert_eq!(a_latest_height, c_latest_height);

        // confirm blocks at this height match c4
        let a3 = node_a.get_block_by_height(c_block4.height).await?;
        let b3 = node_b.get_block_by_height(c_block4.height).await?;
        let c3 = node_c.get_block_by_height(c_block4.height).await?;
        assert_eq!(a3, b3);
        assert_eq!(a3, c3);
    }

    // confirm mempool txs in nodes have remained in the mempool and,
    // confirm that all txs have made it to all peers, regardless of canon status
    // Canonical blocks by mining peer: A1, B2, B3, C4
    {
        let mut peer_b_commitment_txs = vec![peer_b_b2_stake_tx.id(), peer_b_b2_pledge_tx.id()];
        peer_b_commitment_txs.sort();
        let mut peer_c_commitment_txs = vec![peer_c_b2_stake_tx.id(), peer_c_b2_pledge_tx.id()];
        peer_c_commitment_txs.sort();
        let mut all_commitment_txs = peer_b_commitment_txs.clone();
        all_commitment_txs.extend(&peer_c_commitment_txs);
        all_commitment_txs.sort();

        // check txs are in mempools
        node_b
            .wait_for_mempool_commitment_txs(all_commitment_txs.clone(), seconds_to_wait)
            .await
            .expect("node_b and node_c txs to still be on node_b");
        node_c
            .wait_for_mempool_commitment_txs(all_commitment_txs.clone(), seconds_to_wait)
            .await
            .expect("node_b and node_c txs to still be on node_c");

        // sort tx order
        async fn sorted_commitments_at(
            node: &IrysNodeTest<IrysNodeCtx>,
            height: u64,
        ) -> eyre::Result<Vec<H256>> {
            let mut txs = node
                .get_block_by_height(height)
                .await?
                .commitment_tx_ids()
                .to_vec();
            txs.sort();
            Ok(txs)
        }

        // check correct txs made it into specific canon blocks, that are now synced across every node
        assert_eq!(sorted_commitments_at(&node_a, 1).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_a, 2).await?,
            peer_b_commitment_txs
        ); // expect only the two txs included in Peer B B2
        assert_eq!(sorted_commitments_at(&node_a, 3).await?, vec![]);
        // Expect txs that were mined in both c2 (non canonical) and c4 (now canonical)
        // The reason for them being in the 4th block, is that peer C sees them as non canon when it re-orgs after receiving B2 and B3. Therefore then returns as eligible txs
        // To reiterate. These were previously mined in non canon block C2. They were then mined again in canon block C4
        assert_eq!(
            sorted_commitments_at(&node_a, 4).await?,
            peer_c_commitment_txs
        );

        assert_eq!(sorted_commitments_at(&node_b, 1).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_b, 2).await?,
            peer_b_commitment_txs
        );
        assert_eq!(sorted_commitments_at(&node_b, 3).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_b, 4).await?,
            peer_c_commitment_txs
        );

        assert_eq!(sorted_commitments_at(&node_c, 1).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_c, 2).await?,
            peer_b_commitment_txs
        );
        assert_eq!(sorted_commitments_at(&node_c, 3).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_c, 4).await?,
            peer_c_commitment_txs
        );
    }

    // gracefully shutdown nodes
    tokio::join!(node_a.stop(), node_b.stop(), node_c.stop(),);
    Ok(())
}

/// Reorg where there are 3 forks and the tip moves across all of them as each is extended longer than the other.
///   We need to verify that
///    - publish txs are eligible for inclusion in future blocks once they are no longer part of the canonical chain
///    - publish txs do not appear twice, or are missing from canonical chain
///    - tests all canonical blocks move to all peers
///    - tests all the balance changes that were applied in one fork are reverted during the Reorg
///    - tests new balance changes are applied based on the new canonical branch
#[rstest::rstest]
#[case::full_validation(true)]
#[case::default(false)]
#[test_log::test(tokio::test)]
async fn heavy3_reorg_tip_moves_across_nodes_publish_txs(
    #[case] enable_full_validation: bool,
) -> eyre::Result<()> {
    //
    // Stage 0: SETUP AND STARTUP
    //

    let num_blocks_in_epoch = 3;
    let seconds_to_wait = 15;
    const DATA_CHUNK_SIZE: usize = 32;

    // setup config
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = DATA_CHUNK_SIZE as u64;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    genesis_config
        .consensus
        .get_mut()
        .enable_full_ingress_proof_validation = enable_full_validation;

    // create test data
    let data = vec![0_u8; DATA_CHUNK_SIZE];
    let data_chunks = vec![[0_u8; DATA_CHUNK_SIZE]];

    // signers
    let b_signer = genesis_config.new_random_signer();
    let c_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&b_signer, &c_signer]);

    // genesis node / node_a
    let node_a = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // additional configs for peers
    let config_b = node_a.testing_peer_with_signer(&b_signer);
    let config_c = node_a.testing_peer_with_signer(&c_signer);

    let node_b = node_a
        .testing_peer_with_assignments_and_name(config_b, "NODE_B")
        .await?;
    let node_c = node_a
        .testing_peer_with_assignments_and_name(config_c, "NODE_C")
        .await?;

    // Expected state at end of stage 0:
    //  Nodes A, B, C started
    //  Nodes B, C are staked and have partition assignments

    //
    // Stage 1: STARTING STATE CHECKS
    //

    // After testing_peer_with_assignments_and_name, all nodes should be synced
    // and at or past the first epoch boundary with peers B and C staked.
    let base_height = node_a.get_canonical_chain_height().await;
    node_b
        .wait_for_block_at_height(base_height, seconds_to_wait)
        .await?;
    node_c
        .wait_for_block_at_height(base_height, seconds_to_wait)
        .await?;

    // get the block at base_height to use for balance checks
    // Note: After testing_peer_with_assignments_and_name, peers B and C have staked and
    // spent fees, so we capture their balances at base_height (after staking)
    let base_block = node_a.get_block_by_height(base_height).await?;

    // get starting balances after staking (from base_height block)
    let signer_b_starting_balance: U256 = node_a
        .get_balance(b_signer.address(), base_block.evm_block_hash)
        .await;
    let signer_c_starting_balance: U256 = node_a
        .get_balance(c_signer.address(), base_block.evm_block_hash)
        .await;

    // check balances are not zero (they should have remaining balance after staking)
    assert_ne!(U256::from(0), signer_b_starting_balance);
    assert_ne!(U256::from(0), signer_c_starting_balance);

    // Expected state at end of stage 1:
    //  All nodes synced to base_height (past epoch boundary)
    //  Peers B and C are staked in epoch snapshot

    //
    // Stage 2: MINE BLOCK
    //

    // mine a single block, and let everyone sync so future txs use this block's height as anchor.
    node_a.mine_block().await?; // mine block a1
    let block1_height = base_height + 1;
    node_a
        .wait_for_block_at_height(block1_height, seconds_to_wait)
        .await?;
    let a_block1 = node_a.get_block_by_height(block1_height).await?; // get block a1
    node_b
        .wait_for_block(&a_block1.block_hash, seconds_to_wait)
        .await?;
    node_c
        .wait_for_block(&a_block1.block_hash, seconds_to_wait)
        .await?;
    node_b
        .wait_for_block_at_height(block1_height, seconds_to_wait)
        .await?;
    let b_block1 = node_b.get_block_by_height(block1_height).await?; // get block b1
    node_c
        .wait_for_block_at_height(block1_height, seconds_to_wait)
        .await?;
    let c_block1 = node_c.get_block_by_height(block1_height).await?; // get block c1

    assert_eq!(
        a_block1.data_ledgers[DataLedger::Publish].tx_ids.len(),
        0,
        "No publish txs should exist to be included in this block. Ledgers: {:?}",
        a_block1.data_ledgers[DataLedger::Publish].tx_ids
    );
    assert_eq!(
        a_block1.data_ledgers[DataLedger::Submit].tx_ids.len(),
        0,
        "No submit txs should exist to be included in this block. Ledgers: {:?}",
        a_block1.data_ledgers[DataLedger::Submit].tx_ids
    );

    // wait for EVM blocks to be queryable on each node before checking balances
    tokio::try_join!(
        node_a.wait_for_evm_block(a_block1.evm_block_hash, seconds_to_wait),
        node_b.wait_for_evm_block(b_block1.evm_block_hash, seconds_to_wait),
        node_c.wait_for_evm_block(c_block1.evm_block_hash, seconds_to_wait),
    )?;

    // check balances in block 1 are unchanged from genesis
    assert_eq!(
        node_a
            .get_balance(b_signer.address(), a_block1.evm_block_hash)
            .await,
        signer_b_starting_balance
    );
    assert_eq!(
        node_a
            .get_balance(c_signer.address(), a_block1.evm_block_hash)
            .await,
        signer_c_starting_balance
    );
    assert_eq!(
        node_b
            .get_balance(b_signer.address(), b_block1.evm_block_hash)
            .await,
        signer_b_starting_balance
    );
    assert_eq!(
        node_b
            .get_balance(c_signer.address(), b_block1.evm_block_hash)
            .await,
        signer_c_starting_balance
    );
    assert_eq!(
        node_c
            .get_balance(b_signer.address(), c_block1.evm_block_hash)
            .await,
        signer_b_starting_balance
    );
    assert_eq!(
        node_c
            .get_balance(c_signer.address(), c_block1.evm_block_hash)
            .await,
        signer_c_starting_balance
    );

    // check block 1 mining reward is not 0
    assert_ne!(a_block1.reward_amount, U256::from(0_u128));
    assert_ne!(b_block1.reward_amount, U256::from(0_u128));
    assert_ne!(c_block1.reward_amount, U256::from(0_u128));

    // Expected state at end of stage 2:
    //  Nodes A, B, C at block height 1
    //  Signer B, C balance remains at genesis balance
    //  Node A signer has received a block reward but we don't use Node A signer in this test

    //
    // Stage 3: DISABLE ANY/ALL GOSSIP
    //
    node_a.gossip_disable();
    node_b.gossip_disable();
    node_c.gossip_disable();

    // Expected state at end of stage 3:
    // Unchanged from stage 2

    //
    // Stage 4: GENERATE ISOLATED txs
    //

    // node_b generates txs in isolation for inclusion in block 2
    let peer_b_b2_submit_tx = node_b
        .post_data_tx(node_b.get_anchor().await?, data.clone(), &b_signer)
        .await;

    // node_c generates txs in isolation for inclusion block 2
    let peer_c_b2_submit_tx = node_c
        .post_data_tx(node_c.get_anchor().await?, data, &c_signer)
        .await;

    // wait for txs to be in mempools
    node_b
        .wait_for_mempool(peer_b_b2_submit_tx.header.id, seconds_to_wait)
        .await?;
    node_c
        .wait_for_mempool(peer_c_b2_submit_tx.header.id, seconds_to_wait)
        .await?;

    // Expected state at end of stage 4:
    //  Nodes A, B, C remain at block height 1
    //  Signer B, C balance remains at genesis balance
    //  Node C mempool now contains tx peer_b_b2_submit_tx
    //  Node C mempool now contains tx peer_b_b2_submit_tx

    //
    // Stage 5: MINE FORK A and B TO HEIGHT 2 and 3
    //

    // Mine competing blocks on A and B without gossip
    let (a_block2, _, a_block2_txs) = node_a.mine_block_without_gossip().await?; // block a2
    node_a
        .wait_for_block_at_height(a_block2.height, seconds_to_wait)
        .await?;

    let (b_block2, b_block2_payload, b_block2_txs) = node_b.mine_block_without_gossip().await?; // block b2
    node_b
        .wait_for_block_at_height(b_block2.height, seconds_to_wait)
        .await?;

    // post chunks so txs go from submit ledger to publish ledger in block 3
    node_b
        .post_chunk_32b(&peer_b_b2_submit_tx, 0, &data_chunks)
        .await;
    node_c
        .post_chunk_32b(&peer_c_b2_submit_tx, 0, &data_chunks)
        .await;

    // Mine the heightest block on any node so far, on Node B
    let (b_block3, b_block3_payload, b_block3_txs) = node_b.mine_block_without_gossip().await?; // block b3
    node_b
        .wait_for_block_at_height(b_block3.height, seconds_to_wait)
        .await?;

    // check how many txs made it into each block, we expect no more than 1
    assert_eq!(
        b_block2.data_ledgers[DataLedger::Publish].tx_ids.len(),
        0,
        "Expect 0 Publish tx on peer B to be in this block."
    );
    assert_eq!(
        b_block2.data_ledgers[DataLedger::Submit].tx_ids.len(),
        1,
        "Expect 1 Submit txs should be in this block."
    );
    assert_eq!(
        b_block3.data_ledgers[DataLedger::Publish].tx_ids.len(),
        1,
        "Expect 1 Publish tx on peer B to be in this block."
    );
    assert_eq!(
        b_block3.data_ledgers[DataLedger::Submit].tx_ids.len(),
        0,
        "No Submit txs should be in this block."
    );
    assert_eq!(
        a_block2.data_ledgers[DataLedger::Submit].tx_ids.len(),
        0,
        "No txs should have been gossiped back to peer A! {:?}",
        a_block2.data_ledgers[DataLedger::Submit].tx_ids
    );
    assert_eq!(
        a_block2.data_ledgers[DataLedger::Publish].tx_ids.len(),
        0,
        "No txs should have been gossiped back to peer A! {:?}",
        a_block2.data_ledgers[DataLedger::Publish].tx_ids
    );

    // wait for EVM blocks to be queryable before checking balances
    tokio::try_join!(
        node_a.wait_for_evm_block(a_block2.evm_block_hash, seconds_to_wait),
        node_b.wait_for_evm_block(b_block2.evm_block_hash, seconds_to_wait),
        node_b.wait_for_evm_block(b_block3.evm_block_hash, seconds_to_wait),
    )?;

    // check balances in block a2
    assert_eq!(
        node_a
            .get_balance(b_signer.address(), a_block2.evm_block_hash)
            .await,
        signer_b_starting_balance,
        "Address: {:?}",
        b_signer.address()
    );
    assert_eq!(
        node_a
            .get_balance(c_signer.address(), a_block2.evm_block_hash)
            .await,
        signer_c_starting_balance,
        "Address: {:?}",
        c_signer.address()
    );

    // check balances in block b2
    // Only 1 Submit tx from node B should be in block b2
    // Calculate fee components
    let term_charges = irys_types::transaction::fee_distribution::TermFeeCharges::new(
        peer_b_b2_submit_tx.header.term_fee,
        &node_b.node_ctx.config.consensus,
    )?;
    let block_producer_reward = term_charges.block_producer_reward;

    let peer_b_total_fee =
        peer_b_b2_submit_tx.header.term_fee + peer_b_b2_submit_tx.header.perm_fee.unwrap();

    assert_eq!(
        node_b
            .get_balance(b_signer.address(), b_block2.evm_block_hash)
            .await,
        signer_b_starting_balance + b_block2.reward_amount - peer_b_total_fee
            + block_producer_reward,
        "Address: {:?}",
        b_signer.address()
    );
    assert_eq!(
        node_b
            .get_balance(c_signer.address(), b_block2.evm_block_hash)
            .await,
        signer_c_starting_balance,
        "Address: {:?}",
        c_signer.address()
    );

    // check balances in block b3
    // The same Submit tx is promoted to Publish in block b3
    // When a tx gets promoted to publish ledger, the node that gossips ingress proofs gets rewards
    // Since b_signer posted the chunk AND mined the block, they get:
    // 1. The ingress proof reward as the proof provider
    // 2. Block producer continues to get the term fee reward from b2

    // Calculate publish fee rewards if the transaction has perm_fee
    let perm_fee = peer_b_b2_submit_tx.header.perm_fee.unwrap();
    // Calculate publish fee charges for ingress proof rewards
    let number_of_ingress_proofs_total = node_b
        .node_ctx
        .config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
    let publish_charges = irys_types::transaction::fee_distribution::PublishFeeCharges::new(
        perm_fee,
        peer_b_b2_submit_tx.header.term_fee,
        &node_b.node_ctx.config.consensus,
        number_of_ingress_proofs_total,
    )?;
    // b_signer gets the ingress proof reward as they posted the chunk
    // The total ingress proof reward is distributed among proof providers
    let publish_rewards = publish_charges.ingress_proof_reward;

    assert_eq!(
        node_b
            .get_balance(b_signer.address(), b_block3.evm_block_hash)
            .await,
        signer_b_starting_balance + b_block2.reward_amount + b_block3.reward_amount
            - peer_b_total_fee
            + block_producer_reward
            + publish_rewards,
        "Address: {:?}",
        b_signer.address()
    );
    assert_eq!(
        node_b
            .get_balance(c_signer.address(), b_block3.evm_block_hash)
            .await,
        signer_c_starting_balance,
        "Address: {:?}",
        c_signer.address()
    );

    // Expected state at end of stage 5:
    //  Node A is now at block height 2
    //  Node B is now at block height 3
    //  Node C remains at block height 1
    //  Signer B balance is now equal to signer_b_starting_balance + b_block2.reward_amount + b_block3.reward_amount - peer_b_total_fee + block_producer_reward + publish_rewards
    //  Signer C balance remains at genesis balance
    //  Node C mempool now has proof for tx peer_b_b2_submit_tx
    //  Node C mempool now has proof for tx peer_b_b2_submit_tx

    //
    // STAGE 6: NODE B -> Node C
    //
    // send full block to node c. This will include the commitment txs for block 2
    // this will cause a reorg on node c (which is only height 2) to match the chain on node b (height 3)
    // this will cause the txs that were previously canonical from C2 to become non canon
    {
        node_b
            .send_full_block(
                &node_c,
                &b_block2,
                b_block2_payload.clone(),
                b_block2_txs.clone(),
            )
            .await?;
        tracing::error!("posted block 2: {:?}", b_block2.block_hash);
        node_b
            .send_full_block(
                &node_c,
                &b_block3,
                b_block3_payload.clone(),
                b_block3_txs.clone(),
            )
            .await?;
        tracing::error!("posted block 3: {:?}", b_block3.block_hash);

        node_c.wait_for_block(&b_block2.block_hash, 10).await?;
        node_c.wait_for_block(&b_block3.block_hash, 10).await?;
        // check node A has not received blocks from B
        // i.e. that gossip has been disabled and the there is a fork between A and B
        assert!(
            node_a
                .wait_for_block(&b_block2.block_hash, 1)
                .await
                .is_err(),
            "Node A should not yet have received block 2 from Node B"
        );
        assert!(
            node_a
                .wait_for_block(&b_block3.block_hash, 1)
                .await
                .is_err(),
            "Node A should not yet have received block 3 from Node B"
        );
    }
    // Expected state at end of stage 6:
    //  Node A remains at block height 2
    //  Node B remains at block height 3
    //  Node C is now at block height 3 and reorgs having received two blocks from Node B

    //
    // Stage 7: MINE FORK C TO HEIGHT block1_height + 3
    //

    // Node C mines on top of B's chain and does not gossip it back to B
    // Node C has the non canon txs from it's now non canon block 2.
    // Node C will choose to include these txs in block C4
    let block3_height = block1_height + 2;
    let block4_height = block1_height + 3;
    if let Err(does_not_reach_height) = node_c
        .wait_for_block_at_height(block3_height, seconds_to_wait)
        .await
    {
        tracing::error!(
            "Node C Failed to reach block height {}: {:?}",
            block3_height,
            does_not_reach_height
        );
        Err(does_not_reach_height)?
    }
    let (c_block4, c_block4_payload, c_block4_txs) = node_c.mine_block_without_gossip().await?;
    if let Err(does_not_reach_height) = node_c
        .wait_for_block_at_height(block4_height, seconds_to_wait)
        .await
    {
        tracing::error!(
            "Node C Failed to reach block height {}: {:?}",
            block4_height,
            does_not_reach_height
        );
    }
    assert_eq!(
        c_block4.height, block4_height,
        "Node C Failed to reach block height {}",
        block4_height
    ); // block c4

    //
    // Stage 8: FINAL SYNC / RE-ORGs
    //
    {
        // Enable gossip
        node_a.gossip_enable();
        node_b.gossip_enable();
        node_c.gossip_enable();
        if enable_full_validation {
            // Use full block transfer so chunks arrive before validation
            node_b
                .send_full_block(
                    &node_a,
                    &b_block2,
                    b_block2_payload.clone(),
                    b_block2_txs.clone(),
                )
                .await?;
            node_b
                .send_full_block(
                    &node_a,
                    &b_block3,
                    b_block3_payload.clone(),
                    b_block3_txs.clone(),
                )
                .await?;
            node_c
                .send_full_block(
                    &node_a,
                    &c_block4,
                    c_block4_payload.clone(),
                    c_block4_txs.clone(),
                )
                .await?;
            // For full-validation correctness, we only need to guarantee the receiver has chunks for published txs when validating.
            // We use send_full_block for B→A and C→A (those contain Publish txs with proofs),
            // but we use a lighter header delivery for A→B/C to avoid the EVM payload requirement of send_full_block()
            let a_block2_sealed =
                crate::utils::build_sealed_block(a_block2.as_ref().clone(), &a_block2_txs)?;
            node_a
                .send_block_to_peer(&node_b, Arc::clone(&a_block2_sealed))
                .await?;
            node_a
                .send_block_to_peer(&node_c, Arc::clone(&a_block2_sealed))
                .await?;
        } else {
            // Gossip all blocks so everyone syncs
            node_b.gossip_block_to_peers(&b_block2)?;
            node_b.gossip_block_to_peers(&b_block3)?;
            node_c.gossip_block_to_peers(&c_block4)?;
            node_a.gossip_block_to_peers(&a_block2)?;
        }
    }
    //
    // Stage 9: FINAL STATE CHECKS
    //

    // confirm all three nodes are at the same and expected height "4"
    {
        node_a
            .wait_for_block_at_height(c_block4.height, seconds_to_wait)
            .await?;
        node_b
            .wait_for_block_at_height(c_block4.height, seconds_to_wait)
            .await?;
        node_c
            .wait_for_block_at_height(c_block4.height, seconds_to_wait)
            .await?;

        // confirm chain has identical and expected height on all three nodes
        let a_latest_height = node_a.get_canonical_chain_height().await;
        let b_latest_height = node_b.get_canonical_chain_height().await;
        let c_latest_height = node_c.get_canonical_chain_height().await;
        assert_eq!(a_latest_height, c_block4.height);
        assert_eq!(a_latest_height, b_latest_height);
        assert_eq!(a_latest_height, c_latest_height);

        // confirm blocks at this height match c4
        let a3 = node_a.get_block_by_height(c_block4.height).await?;
        let b3 = node_b.get_block_by_height(c_block4.height).await?;
        let c3 = node_c.get_block_by_height(c_block4.height).await?;
        assert_eq!(a3, b3);
        assert_eq!(a3, c3);
    }

    // confirm mempool txs in nodes have remained in the mempool and,
    // confirm that all txs have made it to all peers, regardless of canon status
    // Canonical blocks by mining peer: A1, B2, B3, C4
    {
        let mut peer_b_submit_txs = vec![peer_b_b2_submit_tx.header.id];
        peer_b_submit_txs.sort();
        let mut peer_c_submit_txs = vec![peer_c_b2_submit_tx.header.id];
        peer_c_submit_txs.sort();
        let mut all_submit_txs = peer_b_submit_txs.clone();
        all_submit_txs.extend(&peer_c_submit_txs);
        all_submit_txs.sort();

        // check txs are in mempools
        node_b
            .wait_for_mempool(peer_b_b2_submit_tx.header.id, seconds_to_wait)
            .await
            .expect("node_b txs to still be on node_b");

        node_c
            .wait_for_mempool(peer_c_b2_submit_tx.header.id, seconds_to_wait)
            .await
            .expect("node_c txs to still be on node_c");

        // sort tx order
        async fn sorted_data_txs_at(
            node: &IrysNodeTest<IrysNodeCtx>,
            height: u64,
            ledger: DataLedger,
        ) -> eyre::Result<Vec<H256>> {
            let txs_map: Vec<H256> = node
                .get_block_by_height(height)
                .await?
                .get_data_ledger_tx_ids()
                .get(&ledger)
                .cloned()
                .unwrap_or_default();

            let mut txs: Vec<H256> = txs_map.into_iter().collect();
            txs.sort();

            Ok(txs)
        }

        // check correct txs made it into specific canon blocks, that are now synced across every node
        // Heights are relative to base_height (epoch_height from Stage 1.5)
        let block2_height = block1_height + 1;
        for node in [&node_a, &node_b, &node_c] {
            assert_eq!(
                sorted_data_txs_at(node, block1_height, DataLedger::Submit).await?,
                vec![]
            );
            assert_eq!(
                sorted_data_txs_at(node, block1_height, DataLedger::Publish).await?,
                vec![]
            );
            assert_eq!(
                sorted_data_txs_at(node, block2_height, DataLedger::Submit).await?,
                peer_b_submit_txs
            );
            assert_eq!(
                sorted_data_txs_at(node, block2_height, DataLedger::Publish).await?,
                vec![]
            );
            assert_eq!(
                sorted_data_txs_at(node, block3_height, DataLedger::Submit).await?,
                vec![]
            );
            assert_eq!(
                sorted_data_txs_at(node, block3_height, DataLedger::Publish).await?,
                peer_b_submit_txs // promoted from previous block
            );
            // Expect txs that were mined in both c2 (non canonical) and c4 (now canonical)
            // The reason for them being in the 4th block, is that peer C sees them as non canon when it re-orgs after receiving B2 and B3. Therefore then returns as eligible txs
            // To reiterate. These were previously mined in non canon block C2. They were then mined again in canon block C4
            // As they arrive with proofs in block 4, they appear in both the submit and publish ledgers.
            assert_eq!(
                sorted_data_txs_at(node, block4_height, DataLedger::Submit).await?,
                peer_c_submit_txs
            );

            assert_eq!(
                sorted_data_txs_at(node, block4_height, DataLedger::Publish).await?,
                peer_c_submit_txs
            );
        }

        // wait for EVM blocks to be queryable on node_a after reorg
        tokio::try_join!(
            node_a.wait_for_evm_block(c_block1.evm_block_hash, seconds_to_wait),
            node_a.wait_for_evm_block(c_block4.evm_block_hash, seconds_to_wait),
        )?;

        // re-assert start balances
        assert_eq!(
            node_a
                .get_balance(b_signer.address(), c_block1.evm_block_hash)
                .await,
            signer_b_starting_balance,
        );
        assert_eq!(
            node_a
                .get_balance(c_signer.address(), c_block1.evm_block_hash)
                .await,
            signer_c_starting_balance,
        );
        // assert final balances
        // Calculate fee components for peer C's transaction

        // Calculate publish fee rewards for peer C's transaction if it has perm_fee
        let perm_fee = peer_c_b2_submit_tx.header.perm_fee.unwrap();
        let number_of_ingress_proofs_total = node_c
            .node_ctx
            .config
            .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
        let peer_c_publish_charges =
            irys_types::transaction::fee_distribution::PublishFeeCharges::new(
                perm_fee,
                peer_c_b2_submit_tx.header.term_fee,
                &node_c.node_ctx.config.consensus,
                number_of_ingress_proofs_total,
            )?;
        // c_signer gets the ingress proof reward as they posted the chunk
        let peer_c_publish_rewards = peer_c_publish_charges.ingress_proof_reward;

        // Calculate block producer reward for peer C's transaction
        let peer_c_term_charges = irys_types::transaction::fee_distribution::TermFeeCharges::new(
            peer_c_b2_submit_tx.header.term_fee,
            &node_c.node_ctx.config.consensus,
        )?;
        let peer_c_block_producer_reward = peer_c_term_charges.block_producer_reward;

        assert_eq!(
            node_a
                .get_balance(b_signer.address(), c_block4.evm_block_hash)
                .await,
            signer_b_starting_balance + b_block2.reward_amount + b_block3.reward_amount
                - peer_b_total_fee
                + block_producer_reward
                + publish_rewards, // Include the publish rewards from b_block3
        );
        assert_eq!(
            node_a
                .get_balance(c_signer.address(), c_block4.evm_block_hash)
                .await,
            signer_c_starting_balance + c_block4.reward_amount
                - peer_c_b2_submit_tx.header.total_cost()
                + peer_c_block_producer_reward
                + peer_c_publish_rewards,
        );
    }

    // gracefully shutdown nodes
    tokio::join!(node_a.stop(), node_b.stop(), node_c.stop(),);
    Ok(())
}

/// Two node max block depth re-org test
/// Gossip disabled after block 1. Peer A mines 2 blocks of the migration depth
/// while peer B mines one block short. This creates the longest possible
/// fork without triggering block migration. Once gossip is re-enabled peer A should
/// reorg to peer B's chain.
#[test_log::test(tokio::test)]
async fn slow_heavy3_reorg_upto_block_migration_depth() -> eyre::Result<()> {
    // config variables
    // Adjust num_blocks_in_epoch to control how many blocks are mined for the reorg
    let num_blocks_in_epoch = 10;
    let seconds_to_wait = 60;

    // setup config
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    // signers
    let b_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&b_signer]);

    // genesis node / node_a
    let node_a = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // additional configs for peers
    let config_b = node_a.testing_peer_with_signer(&b_signer);

    // start peer nodes
    let node_b = IrysNodeTest::new(config_b)
        .start_and_wait_for_packing("NODE_B", seconds_to_wait)
        .await;

    //
    // Stage 1: STARTING STATE CHECKS
    //

    // check peer heights match genesis - i.e. that we are all in sync on block 0
    let current_height = node_a.get_canonical_chain_height().await;
    assert_eq!(current_height, 0);
    node_b
        .wait_for_block_at_height(current_height, seconds_to_wait)
        .await?;

    //
    // Stage 2: MINE BLOCK
    //

    // mine a single block, and let everyone sync so future txs start at block height 1.
    node_a.mine_block().await?; // mine block a1
    node_a.wait_for_block_at_height(1, seconds_to_wait).await?;
    let block_height_1 = node_a.get_block_by_height(1).await?; // get block a1
    node_b.wait_for_block_at_height(1, seconds_to_wait).await?;

    assert_eq!(
        block_height_1.system_ledgers.len(),
        0,
        "No txs should exist to be included in this block"
    );

    //
    // Stage 3: DISABLE ANY/ALL GOSSIP
    //
    {
        node_a.gossip_disable();
        node_b.gossip_disable();
    }

    //
    // Stage 4: GENERATE ISOLATED txs
    //

    let anchor = node_b.get_anchor().await?;
    // node_b generates txs in isolation for inclusion in block 2
    let peer_b_b2_stake_tx = node_b
        .post_stake_commitment_without_gossip(Some(anchor))
        .await?;
    let peer_b_b2_pledge_tx = node_b
        .post_pledge_commitment_without_gossip(Some(anchor))
        .await?;

    //
    // Stage 5: MINE FORK A and B TO HEIGHT block_migration_depth - 1 and
    // block_migration_depth
    //

    let blocks_a_to_mine = block_migration_depth - 2;
    let blocks_b_to_mine = block_migration_depth - 1;

    // Mine competing blocks on A and B without gossip
    let mut a_blocks = Vec::with_capacity(blocks_a_to_mine);
    for _ in 0..blocks_a_to_mine {
        let (block, _, _) = node_a.mine_block_without_gossip().await?;
        a_blocks.push(block);
    }
    let mut b_blocks = Vec::with_capacity(blocks_b_to_mine);
    for _ in 0..blocks_b_to_mine {
        let (block, _, _) = node_b.mine_block_without_gossip().await?;
        b_blocks.push(block);
    }

    // confirm the chains have forked
    for (a_block, b_block) in a_blocks.iter().zip(b_blocks.iter()) {
        assert_ne!(a_block, b_block);
    }

    // For convenience keep references to the first and last blocks on each chain
    let a_block2 = a_blocks[0].clone();
    let b_block2 = b_blocks[0].clone();
    let a_last = a_blocks.last().expect("a_blocks not empty");
    let mut b_last = b_blocks.last().expect("b_blocks not empty").clone();

    // False assumption to avoid: one extra block is not always heavier when
    // per-block difficulty varies. Extend B until it is strictly heavier.
    let mut extra_private_blocks = 0_usize;
    while b_last.cumulative_diff <= a_last.cumulative_diff {
        let (block, _, _) = node_b.mine_block_without_gossip().await?;
        b_last = block.clone();
        b_blocks.push(block);
        extra_private_blocks += 1;
        eyre::ensure!(
            extra_private_blocks <= 6,
            "B fork failed to exceed A cumulative difficulty after {} extra block(s): a_last_diff={} b_last_diff={}",
            extra_private_blocks,
            a_last.cumulative_diff,
            b_last.cumulative_diff
        );
    }

    debug!(
        "fork heaviness before sync: a_last_height={} a_last_diff={} b_last_height={} b_last_diff={} extra_b_blocks={}",
        a_last.height,
        a_last.cumulative_diff,
        b_last.height,
        b_last.cumulative_diff,
        extra_private_blocks
    );

    // check how many txs made it into each block, we expect no more than 2
    assert_eq!(
        b_block2.system_ledgers.len(),
        1,
        "Expect 1 of the 2 isolated txs on peer B to be in this block. The stake tx and not the pledge tx."
    );
    assert_eq!(
        a_block2.system_ledgers.len(),
        0,
        "No txs should have been gossiped back to peer A"
    ); // 0 commitments, also means 0 system ledgers

    //
    // Stage 6: FINAL SYNC / RE-ORGs
    //
    let reorg_future = node_a.wait_for_reorg(seconds_to_wait);

    {
        // Enable gossip
        node_a.gossip_enable();
        node_b.gossip_enable();
        // Gossip all blocks so everyone syncs
        for block in &b_blocks {
            node_b.gossip_block_to_peers(block)?;
        }
        for block in &a_blocks {
            node_a.gossip_block_to_peers(block)?;
        }
    }
    let reorg_event = reorg_future.await?;
    debug!("reorg result in migration-depth test: {:?}", reorg_event);

    node_a
        .wait_for_block(&b_last.block_hash, seconds_to_wait)
        .await?;
    node_b
        .wait_for_block(&b_last.block_hash, seconds_to_wait)
        .await?;

    //
    // Stage 7: FINAL STATE CHECKS
    //

    // confirm both nodes are at the same and expected height
    node_a
        .wait_for_block_at_height(b_last.height, seconds_to_wait)
        .await?;
    node_b
        .wait_for_block_at_height(b_last.height, seconds_to_wait)
        .await?;

    // confirm chain has identical and expected height on all nodes
    let a_latest_height = node_a.get_canonical_chain_height().await;
    let b_latest_height = node_b.get_canonical_chain_height().await;
    assert_eq!(a_latest_height, b_latest_height);

    // confirm blocks at this height match on both nodes
    let a_final = node_a.get_block_by_height(b_last.height).await?;
    let b_final = node_b.get_block_by_height(b_last.height).await?;
    assert_eq!(a_final, b_final);

    // confirm mempool txs in nodes have remained in the mempool and,
    // confirm that all txs have made it to all peers, regardless of canon status
    // Canonical blocks by mining peer: A1, B2, B3, C4
    {
        let mut peer_b_commitment_txs = vec![peer_b_b2_stake_tx.id(), peer_b_b2_pledge_tx.id()];
        peer_b_commitment_txs.sort();
        let mut all_commitment_txs = peer_b_commitment_txs.clone();
        all_commitment_txs.sort();

        // check txs are in mempools
        node_b
            .wait_for_mempool_commitment_txs(all_commitment_txs.clone(), seconds_to_wait)
            .await
            .expect("node_b and node_c txs to still be on node_b");

        // sort tx order
        async fn sorted_commitments_at(
            node: &IrysNodeTest<IrysNodeCtx>,
            height: u64,
        ) -> eyre::Result<Vec<H256>> {
            let mut txs = node
                .get_block_by_height(height)
                .await?
                .commitment_tx_ids()
                .to_vec();
            txs.sort();
            Ok(txs)
        }

        // check correct txs made it into specific canon blocks, that are now synced across every node
        assert_eq!(sorted_commitments_at(&node_a, 1).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_a, 2).await?,
            peer_b_commitment_txs
        );
        assert_eq!(sorted_commitments_at(&node_a, 3).await?, vec![]);
        assert_eq!(sorted_commitments_at(&node_b, 1).await?, vec![]);
        assert_eq!(
            sorted_commitments_at(&node_b, 2).await?,
            peer_b_commitment_txs
        );
        assert_eq!(sorted_commitments_at(&node_b, 3).await?, vec![]);
        //also check final blocks for good measure, they should have no txs as we didn't post any after block 2
        assert_eq!(
            sorted_commitments_at(&node_a, a_final.height).await?,
            vec![]
        );
        assert_eq!(
            sorted_commitments_at(&node_b, b_final.height).await?,
            vec![]
        );
    }

    // gracefully shutdown nodes
    tokio::join!(node_a.stop(), node_b.stop());
    Ok(())
}
