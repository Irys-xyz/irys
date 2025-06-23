use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;

#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_reth_block_gets_rejected() -> eyre::Result<()> {
    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.start_public_api().await;
    let peer_node = genesis_node
        .testnet_peer_with_assignments(&peer_signer)
        .await;

    {
        // peer_node.gossip_block(block_header)
        // todo mine a new invalid block on the peer node
        // ensure that the evm block does not have block reward system tx present
    }

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
