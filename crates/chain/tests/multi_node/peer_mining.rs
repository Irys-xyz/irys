use crate::utils::IrysNodeTest;
use irys_types::{NodeConfig, H256};

#[test_log::test(actix_web::test)]
async fn heavy_peer_mining_test() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 7;
    let seconds_to_wait = 20;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    // Create a signer (keypair) for the peer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Initialize the peer with our keypair/signer
    let peer_config = genesis_node.testnet_peer_with_signer(&peer_signer);

    // Start the peer: No packing on the peer, it doesn't have partition assignments yet
    let peer_node = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER")
        .await;
    peer_node.start_public_api().await;

    // Post stake + pledge commitments to the peer
    let stake_tx = peer_node.post_stake_commitment(H256::zero()).await; // zero() is the genesis block hash
    let pledge_tx = peer_node.post_pledge_commitment(H256::zero()).await;

    // Wait for commitment tx to show up in the genesis_node's mempool
    genesis_node
        .wait_for_mempool(stake_tx.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(pledge_tx.id, seconds_to_wait)
        .await?;

    // Mine a block to get the commitments included
    genesis_node.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks
    genesis_node.mine_blocks(block_migration_depth).await?;
    genesis_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;

    // Get the genesis nodes view of the peers assignments
    let peer_assignments = genesis_node.get_partition_assignments(peer_signer.address());

    // Verify that one partition has been assigned to the peer to match its pledge
    assert_eq!(peer_assignments.len(), 1);

    // Wait for the peer to receive & process the epoch block
    let _block_hash = peer_node
        .wait_until_height(num_blocks_in_epoch.try_into()?, seconds_to_wait)
        .await?;
    peer_node.wait_for_packing(seconds_to_wait).await;

    // Verify that the peer has the same view of its own assignments
    let peer_assignments_on_peer = peer_node.get_partition_assignments(peer_signer.address());

    // Verify the peer has the same view of assignments as the genesis node
    assert_eq!(peer_assignments_on_peer.len(), 1);
    assert_eq!(peer_assignments_on_peer[0], peer_assignments[0]);

    // Mine two more blocks on the peer to trigger an epoch
    peer_node.mine_blocks(block_migration_depth).await?;

    // Validate the genesis node processes the peers blocks without errors
    let _block_hash = genesis_node.wait_until_height(4, seconds_to_wait).await?;

    // Mine one more block on the genesis_node to make sure reth state is syncing
    genesis_node.mine_block().await?;

    // Wind down test
    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
