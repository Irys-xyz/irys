use crate::utils::IrysNodeTest;
use irys_testing_utils::*;
use irys_types::{NodeConfig, H256};
use tracing::debug;

#[actix_web::test]
async fn heavy_fork_recovery_epoch_test() -> eyre::Result<()> {
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
    let epoch_block = genesis_node.mine_block().await.unwrap();

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

    // Now it's time to create different epoch timelines for each peers fork
    let _pledge1 = peer1_node
        .post_pledge_commitment_without_gossip(epoch_block.block_hash)
        .await;
    let fork1_3 = peer1_node.mine_block_without_gossip().await?;

    let _pledge2 = peer2_node
        .post_pledge_commitment_without_gossip(epoch_block.block_hash)
        .await;
    let fork2_3 = peer2_node.mine_block_without_gossip().await?;

    // Make sure the blocks on each for have different hashes
    assert_ne!(fork1_3.0.block_hash, fork2_3.0.block_hash);
    debug!(
        fork1_3_height = fork1_3.0.height,
        fork2_3_height = fork2_3.0.height,
        "Comparing fork heights"
    );

    // Mine a block with gossip on one of the peers to extend the chain on genesis to have the peers epoch block
    peer1_node.gossip_block(&fork1_3.0)?;
    let genesis_hash = genesis_node
        .wait_until_height(fork1_3.0.height, seconds_to_wait)
        .await?;
    assert_eq!(genesis_hash, fork1_3.0.block_hash);

    // Have peer1 and peer2 both mine their 4th block, but don't gossip peer2s
    let _peer2_epoch = peer2_node.mine_block_without_gossip().await?.0;
    let peer1_epoch = peer1_node.mine_block().await.unwrap();

    // Wait for peer1_epoch block to arrive at genesis node
    let genesis_epoch_hash = genesis_node.wait_until_height(4, seconds_to_wait).await?;
    assert_eq!(peer1_epoch.block_hash, genesis_epoch_hash);

    // Then extend peer2's chain to be the longest
    peer2_node.mine_blocks(1).await?;
    let peer2_hash = peer2_node.wait_until_height(5, seconds_to_wait).await?;
    let peer2_head = peer2_node.get_block_by_hash(&peer2_hash)?;

    // Verify the height 4 block on peer2's chain is not the height 4 on genesis
    assert_ne!(peer2_head.previous_block_hash, genesis_epoch_hash);

    // Validate that the genesis node reorgs the epoch/commitment state correctly

    // Wind down test
    genesis_node.stop().await;
    peer1_node.stop().await;
    peer2_node.stop().await;
    Ok(())
}
