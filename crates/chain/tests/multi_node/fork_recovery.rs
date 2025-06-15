use crate::utils::IrysNodeTest;
use base58::ToBase58 as _;
use irys_testing_utils::*;
use irys_types::{DataLedger, IrysTransaction, NodeConfig, H256};
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
    let seconds_to_wait = 10;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

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
    peer1_node.wait_until_height(2, seconds_to_wait).await?;
    peer2_node.wait_until_height(2, seconds_to_wait).await?;

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
        .post_storage_tx(
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
        .post_storage_tx_without_gossip(H256::zero(), data1, &peer1_signer)
        .await;
    let peer2_tx = peer2_node
        .post_storage_tx_without_gossip(H256::zero(), data2, &peer2_signer)
        .await;

    // Mine mine blocks on both peers in parallel
    let (result1, result2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1)
    );

    // Fail the test on any error results
    result1?;
    result2?;

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

    // Wait for gossip, may need a better way to do this.
    // Possibly ask the block tree if it has the other block_hash?
    tokio_sleep(5).await;

    let peer1_block_after = peer1_node.get_block_by_height(3).await?;
    let peer2_block_after = peer2_node.get_block_by_height(3).await?;

    // Verify neither peer changed their blocks after receiving the other peers block
    // for the same height.
    assert_eq!(peer1_block_after.block_hash, peer1_block.block_hash);
    assert_eq!(peer2_block_after.block_hash, peer2_block.block_hash);

    genesis_node.wait_until_height(3, seconds_to_wait).await?;
    let genesis_block = genesis_node.get_block_by_height(3).await?;

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
    genesis_node.stop().await;
    peer1_node.stop().await;
    peer2_node.stop().await;
    Ok(())
}
