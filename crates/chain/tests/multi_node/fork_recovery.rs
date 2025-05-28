use crate::utils::IrysNodeTest;
use base58::ToBase58;
use irys_testing_utils::*;
use irys_types::{NodeConfig, H256};
use tracing::debug;

#[actix_web::test]
async fn heavy_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
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

    // Mine mine blocks on both peers in parallel
    let (result1, result2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1)
    );
    result1?;
    result2?;

    let peer1_block = peer1_node.get_block_by_height(3).await?;
    let peer2_block = peer2_node.get_block_by_height(3).await?;

    // Assert both blocks have the same cumulative difficulty this will ensure
    // that the peers prefer the first block they saw with this cumulative difficulty,
    // their own.
    assert_eq!(peer1_block.cumulative_diff, peer2_block.cumulative_diff);

    peer2_node.gossip_block(&peer2_block).await?;
    peer1_node.gossip_block(&peer1_block).await?;

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
    debug!("\nGENESIS: {:?}", genesis_block.block_hash.0.to_base58());

    // Whichever block the genesis node has, have the opposite peer mine the next
    // to force a reorg
    let reorg_block = if genesis_block.block_hash == peer1_block.block_hash {
        peer2_node.mine_block().await?;
        peer2_node.get_block_by_height(4).await?
    } else {
        peer1_node.mine_block().await?;
        peer1_node.get_block_by_height(4).await?
    };

    // GENESIS occasionally doesn't arrive at block 4 - 10s for gossip is too slow!
    genesis_node.wait_until_height(4, seconds_to_wait).await?;
    let genesis_block = genesis_node.get_block_by_height(4).await?;

    debug!(
        "reorg_block: {}\nnew_genesis: {}",
        reorg_block.block_hash.0.to_base58(),
        genesis_block.block_hash.0.to_base58()
    );

    // Wind down test
    genesis_node.stop().await;
    peer1_node.stop().await;
    peer2_node.stop().await;
    Ok(())
}
