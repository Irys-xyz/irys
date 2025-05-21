use irys_testing_utils::*;
use irys_types::{NodeConfig, H256};
use tracing::debug;

use crate::utils::IrysNodeTest;

#[actix_web::test]
async fn heavy_peer_mining_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);

    // Create a signer (keypair) for the peer and fund it
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing(10)
        .await;
    genesis_node.start_public_api().await;

    // Initialize the peer with our keypair/signer
    let peer_config = genesis_node.testnet_peer_with_signer(&peer_signer);

    // Start the peer: No packing on the peer, it doesn't have partition assignments yet
    let peer_node = IrysNodeTest::new(peer_config.clone()).start().await;
    peer_node.start_public_api().await;
    debug!("{:#?}", peer_node.node_ctx.config.node_config);

    let stake_tx = peer_node.post_stake_commitment(H256::zero()).await; // zero() is the genesis block hash
    let pledge_tx = peer_node.post_pledge_commitment(H256::zero()).await;

    genesis_node.wait_for_mempool(stake_tx.id, 10).await?;
    genesis_node.wait_for_mempool(pledge_tx.id, 10).await?;

    genesis_node.mine_block().await.unwrap();
    let block = genesis_node.get_block_by_height(1).await.unwrap();
    debug!("SystemLedgers: {:#?}", block.system_ledgers);

    genesis_node.stop().await;
    peer_node.stop().await;

    Ok(())
}
