use crate::utils::IrysNodeTest;
use base58::ToBase58 as _;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::*;
use irys_types::{DataLedger, IrysTransaction, NodeConfig, PeerAddress, RethPeerInfo, H256};
use tracing::debug;

#[actix_web::test]
async fn heavy_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=none;irys_p2p::server=none;irys_actors::mining=error",
    );
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 15;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
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
    genesis_node.start_public_api().await;

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis_node.testnet_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testnet_peer_with_signer(&peer2_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;
    peer1_node.start_public_api().await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;
    peer2_node.start_public_api().await;

    // Post stake + pledge commitments to peer1
    let peer1_stake_tx = peer1_node.post_stake_commitment(H256::zero()).await; // zero() is the genesis block hash
    let peer1_pledge_tx = peer1_node.post_pledge_commitment(H256::zero()).await;

    // Post stake + pledge commitments to peer2
    let peer2_stake_tx = peer2_node.post_stake_commitment(H256::zero()).await;
    let peer2_pledge_tx = peer2_node.post_pledge_commitment(H256::zero()).await;

    // Wait for all commitment tx to show up in the genesis_node's mempool
    genesis_node
        .wait_for_mempool(peer1_stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer1_pledge_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_pledge_tx.id, seconds_to_wait)
        .await?;

    // Mine a block to get the commitments included
    genesis_node.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    genesis_node.mine_block().await.unwrap();

    // wait for block mining to reach tree height
    genesis_node.wait_until_height(2, seconds_to_wait).await?;
    // wait for migration to reach index height
    genesis_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    // Get the genesis nodes view of the peers assignments
    let peer1_assignments = genesis_node
        .get_partition_assignments(peer1_signer.address())
        .await;
    let peer2_assignments = genesis_node
        .get_partition_assignments(peer2_signer.address())
        .await;

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
            H256::zero(),
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
        .post_data_tx_without_gossip(H256::zero(), data1, &peer1_signer)
        .await;
    let peer2_tx = peer2_node
        .post_data_tx_without_gossip(H256::zero(), data2, &peer2_signer)
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
    peer1_node.wait_until_height(3, seconds_to_wait).await?;
    peer2_node.wait_until_height(3, seconds_to_wait).await?;
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

    peer2_node.gossip_block(&peer2_block)?;
    peer1_node.gossip_block(&peer1_block)?;

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
    genesis_node.wait_until_height(3, seconds_to_wait).await?;
    let genesis_block = genesis_node.get_block_by_height(3).await?;
    //wait for genesis block index height 2
    // FIXME: genesis_node.wait_until_height_on_chain(2) sometimes fails
    genesis_node
        .wait_until_block_index_height(2, seconds_to_wait)
        .await
        .expect("expected genesis to index block 2");

    debug!(
        "\nPEER1\n    before: {} c_diff: {}\n    after:  {} c_diff: {}\nPEER2\n    before: {} c_diff: {}\n    after:  {} c_diff: {}",
        peer1_block.block_hash.0.to_base58(),
        peer1_block.cumulative_diff,
        peer1_block_after.block_hash.0.to_base58(),
        peer1_block_after.cumulative_diff,
        peer2_block.block_hash.0.to_base58(),
        peer2_block.cumulative_diff,
        peer2_block_after.block_hash.0.to_base58(),
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
    let reorg_tx: IrysTransaction;
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

    let old_fork_hashes: Vec<_> = reorg_event.old_fork.iter().map(|b| b.block_hash).collect();
    let new_fork_hashes: Vec<_> = reorg_event.new_fork.iter().map(|b| b.block_hash).collect();

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
        .map(|bh| bh.block_hash)
        .collect();

    let new_fork: Vec<_> = reorg_event
        .new_fork
        .iter()
        .map(|bh| bh.block_hash)
        .collect();

    println!("\nfork_parent: {:?}", reorg_event.fork_parent.block_hash);
    println!("old_fork:\n  {:?}", old_fork);
    println!("new_fork:\n  {:?}", new_fork);

    assert_eq!(old_fork, vec![canon_before.0.last().unwrap().block_hash]);
    assert_eq!(
        new_fork,
        vec![
            canon.0[canon.0.len() - 2].block_hash,
            canon.0.last().unwrap().block_hash
        ]
    );

    assert_eq!(reorg_event.new_tip, *new_fork.last().unwrap());

    // Wind down test
    tokio::join!(genesis_node.stop(), peer1_node.stop(), peer2_node.stop());
    Ok(())
}

/// todo: A fork builds on top of a fork
/// Reorg where there are 3 forks and the tip moves across all of them as each is extended longer than the other.
///   We need to verify that
///    - all the balance changes that were applied in one fork are reverted during the Reorg
///    - new balance changes are applied based on the new canonical branch
#[test_log::test(actix_web::test)]
async fn heavy_reorg_tip_moves_across_nodes() -> eyre::Result<()> {
    initialize_tracing();
    // config variables
    let num_blocks_in_epoch = 5; // test currently mines 4 blocks, and expects txs to remain in mempool
    let seconds_to_wait = 15;

    // setup config
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
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
    let config_b = node_a.testnet_peer_with_signer(&c_signer);
    let config_c = node_a.testnet_peer_with_signer(&b_signer);

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
    let current_height = node_a.get_height().await;
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
    let a_block1 = node_a.get_block_by_height(1).await?; // get block a1
    node_b
        .wait_for_block(&a_block1.block_hash, seconds_to_wait)
        .await?;
    node_c
        .wait_for_block(&a_block1.block_hash, seconds_to_wait)
        .await?;
    let b_block1 = node_b.get_block_by_height(1).await?; // get block b1
    let c_block1 = node_c.get_block_by_height(1).await?; // get block c1

    assert_eq!(
        a_block1.system_ledgers.len(),
        0,
        "No txs should exist to be included in this block"
    );

    //
    // Stage 3: DISABLE ANY/ALL GOSSIP
    //
    {
        //FIXME: In future this "workaround" of using the syncing state to prevent gossip
        //       broadcasts can be replaced with something more appropriate and correctly named
        node_a.node_ctx.sync_state.set_is_syncing(true);
        node_b.node_ctx.sync_state.set_is_syncing(true);
        node_c.node_ctx.sync_state.set_is_syncing(true);
    }

    //
    // Stage 4: GENERATE ISOLATED txs
    //

    // node_b generates txs in isolation for inclusion in block 2
    let peer_b_b2_stake_tx = node_b
        .post_stake_commitment_without_gossip(b_block1.block_hash)
        .await;
    let peer_b_b2_pledge_tx = node_b
        .post_pledge_commitment_without_gossip(peer_b_b2_stake_tx.id)
        .await;

    // node_c generates txs in isolation for inclusion block 2
    let peer_c_b2_stake_tx = node_c
        .post_stake_commitment_without_gossip(c_block1.block_hash)
        .await;
    let peer_c_b2_pledge_tx = node_c
        .post_pledge_commitment_without_gossip(peer_c_b2_stake_tx.id)
        .await;

    //
    // Stage 5: MINE FORK A and B TO HEIGHT 2 and 3
    //

    // Mine competing blocks on A and B without gossip
    let (a_block2, _) = node_a.mine_block_without_gossip().await?; // block a2
    let (b_block2, _) = node_b.mine_block_without_gossip().await?; // block b2
    let (b_block3, _) = node_b.mine_block_without_gossip().await?; // block b3

    // check how many txs made it into each block, we expect no more than 2
    tracing::error!("peer_b_b2_stake_tx: {:?}", peer_b_b2_stake_tx);
    tracing::error!("peer_b_b2_pledge_tx: {:?}", peer_b_b2_pledge_tx);
    tracing::error!("peer_c_b2_stake_tx: {:?}", peer_c_b2_stake_tx);
    tracing::error!("peer_c_b2_pledge_tx: {:?}", peer_c_b2_pledge_tx);
    assert_eq!(
        b_block2.system_ledgers.len(),
        1,
        "Expect 1 of the 2 isolated txs on peer B to be in this block. The stake tx and not the pledge tx."
    );
    assert_eq!(
        a_block2.system_ledgers.len(),
        0,
        "No txs should have been gossiped back to peer A! {:?}",
        a_block2.system_ledgers[0].tx_ids
    ); // 0 commitments, also means 0 system ledgers

    let peer_c = PeerAddress {
        gossip: format!(
            "{}:{}",
            &node_c.node_ctx.config.node_config.gossip.bind_ip,
            &node_c.node_ctx.config.node_config.gossip.bind_port
        )
        .parse()
        .expect("valid socket address"),
        api: format!(
            "{}:{}",
            &node_c.node_ctx.config.node_config.http.bind_ip,
            &node_c.node_ctx.config.node_config.http.bind_port
        )
        .parse()
        .expect("valid socket address"),
        execution: RethPeerInfo::default(),
    };

    // NODE B -> Node C
    // post commitment txs and then the blocks to node c
    // this will cause a reorg on node c (which is only height 2) to match the chain on node b (height 3)
    // this will cause the txs that were previosuly canonical from C2 to become non canon
    {
        node_c.post_commitment_tx(&peer_b_b2_stake_tx).await;
        node_c.post_commitment_tx(&peer_b_b2_pledge_tx).await;
        node_b.post_block_to_peer(&peer_c, &b_block2).await?;
        tracing::error!("posted block 2: {:?}", b_block2.block_hash);
        node_b.post_block_to_peer(&peer_c, &b_block3).await?;
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
    let (c_block4, _) = node_c.mine_block_without_gossip().await?;
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
        //FIXME: In future this "workaround" of using the syncing state to prevent gossip
        //       broadcasts can be replaced with something more appropriate and correctly named
        node_a.node_ctx.sync_state.set_is_syncing(false);
        node_b.node_ctx.sync_state.set_is_syncing(false);
        node_c.node_ctx.sync_state.set_is_syncing(false);
        // Gossip all blocks so everyone syncs
        node_b.gossip_block(&b_block2)?;
        node_b.gossip_block(&b_block3)?;
        node_c.gossip_block(&c_block4)?;
        node_a.gossip_block(&a_block2)?;
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
        let a_latest_height = node_a.get_height().await;
        let b_latest_height = node_b.get_height().await;
        let c_latest_height = node_c.get_height().await;
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
        let mut peer_b_commitment_txs = vec![peer_b_b2_stake_tx.id, peer_b_b2_pledge_tx.id];
        peer_b_commitment_txs.sort();
        let mut peer_c_commitment_txs = vec![peer_c_b2_stake_tx.id, peer_c_b2_pledge_tx.id];
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
            .expect("node_c and node_c txs to still be on node_c");

        // sort tx order
        async fn sorted_commitments_at(
            node: &IrysNodeTest<IrysNodeCtx>,
            height: u64,
        ) -> eyre::Result<Vec<H256>> {
            let mut txs = node
                .get_block_by_height(height)
                .await?
                .get_commitment_ledger_tx_ids();
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
        // The reason for them being in the 4th block, is that peer C sees them as non canon when it re-orgs after receiving B2 and B3. Therefore then returns as elligible txs
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
