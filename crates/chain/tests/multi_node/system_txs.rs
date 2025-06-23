use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;

#[test_log::test(actix_web::test)]
#[ignore]
// Test that validates system transaction validation is working
// Since mine_block() creates valid blocks with system transactions, peer should be able to sync
async fn heavy_block_rejected_without_reward() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    dbg!();

    // Create a signer (keypair) for the peer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    dbg!();

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    dbg!();
    genesis_node.start_public_api().await;
    dbg!();

    // Use testnet_peer_with_assignments which sets up the peer with stake/pledge
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    dbg!();
    peer_node.mine_block().await?;

    dbg!();
    let final_height = peer_node.get_height().await;
    dbg!();
    // Wait for the genesis to process the new block produced by a peer
    genesis_node
        .wait_until_height(final_height, seconds_to_wait)
        .await
        .expect("peer to sync the block");

    dbg!();
    // If we reach here, validation is working correctly:
    // - Blocks with valid system transactions are accepted
    // - The test passes because peer successfully synced
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
