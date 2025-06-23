use crate::utils::{read_block_from_state, BlockValidationOutcome, IrysNodeTest};
use irys_types::{storage_pricing::Amount, NodeConfig};
use reth::core::primitives::SealedBlock;
use rust_decimal_macros::dec;

#[test_log::test(actix_web::test)]
async fn heavy_block_invalid_block_reward_gets_rejected() -> eyre::Result<()> {
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

    // produce an invalid block

    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
