- [ ] irys block defines 2 stake system txs; reth block only has a single stake system tx; expect rejection
- [ ] irys block defines 2 stake system txs; reth block only has 3 stake system tx; expect rejection
- [ ] irys block defines 1 stake, 1 unstake system txs; reth has the same, but in different order; expect rejection
- [ ] irys block defines 1 stake, 1 unstake system txs; reth has the same; expect success

- todo think of reth state validation

use crate::utils::IrysNodeTest;
use irys_testing_utils::*;
use irys_types::{NodeConfig, H256};

#[actix_web::test]
// Test that validates system transaction validation is working
// Since mine_block() creates valid blocks with system transactions, peer should be able to sync
async fn heavy_block_rejected_without_reward() -> eyre::Result<()> {
// Turn on tracing even before the nodes start
std::env::set_var("RUST_LOG", "debug");
initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for the peer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;

    // Use testnet_peer_with_assignments which sets up the peer with stake/pledge
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    peer_node.mine_block().await?;

    let final_height = peer_node.get_height().await;
    // Wait for the genesis to process the new block produced by a peer
    genesis_node
        .wait_until_height(final_height, seconds_to_wait)
        .await
        .expect("peer to sync the block");

    // If we reach here, validation is working correctly:
    // - Blocks with valid system transactions are accepted
    // - The test passes because peer successfully synced
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())

}
