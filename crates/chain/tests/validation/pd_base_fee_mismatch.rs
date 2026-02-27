use crate::utils::{read_block_from_state, BlockValidationOutcome, IrysNodeTest};
use irys_types::{storage_pricing::Amount, NodeConfig};
use rust_decimal_macros::dec;

/// Test that blocks with incorrect PD base fee are rejected.
///
/// This test verifies that when two nodes have different consensus parameters
/// (specifically different `base_fee_floor` values), a block produced by one node
/// will be rejected by the other node during validation.
///
/// The test works by:
/// 1. Creating a genesis node with a lower base_fee_floor (0.001)
/// 2. Creating a peer node with a higher base_fee_floor (0.01)
/// 3. Having the peer mine a block (which calculates PD base fee using its higher floor)
/// 4. Gossiping that block to the genesis node
/// 5. Verifying the genesis node rejects the block because the PD base fee doesn't match
///    what the genesis node would have calculated with its lower floor
#[test_log::test(actix_web::test)]
async fn heavy_test_block_with_incorrect_pd_base_fee_gets_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;

    // Create genesis node with lower base_fee_floor
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 256 * 1024; // 256 KB

    // Set a low base_fee_floor (0.001 USD per MB)
    let genesis_base_fee_floor = Amount::token(dec!(0.001))?;
    genesis_config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing")
        .base_fee_floor = genesis_base_fee_floor;

    // Create and fund a signer for transactions
    let pd_tx_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // Start genesis node (Node A)
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create peer node (Node B) with a DIFFERENT base_fee_floor
    // Following the exact pattern from the example test:
    // 1. Create peer with matching config and wait for assignments
    // 2. Stop the peer
    // 3. Modify the config
    // 4. Restart the peer
    let peer_node = {
        let peer_node = genesis_node
            .testing_peer_with_assignments(&pd_tx_signer)
            .await?;
        let mut peer_node = peer_node.stop().await;

        // Set a HIGHER base_fee_floor (0.01 USD per MB, 10x higher than genesis)
        // This will cause the peer to calculate a different PD base fee
        let peer_base_fee_floor = Amount::token(dec!(0.01))?;
        peer_node
            .cfg
            .consensus
            .get_mut()
            .hardforks
            .sprite
            .as_mut()
            .expect("Sprite hardfork must be configured for testing")
            .base_fee_floor = peer_base_fee_floor;

        peer_node.start_with_name("peer").await
    };

    // Wait for peer to fully initialize after restart
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Mine a block on the peer node with its different base_fee_floor
    // This block will contain a PdBaseFeeUpdate shadow transaction calculated
    // using the peer's higher floor value
    let (block, block_eth_payload, _) = peer_node.mine_block_without_gossip().await?;

    // Gossip the block to the genesis node
    peer_node.gossip_block_to_peers(&block)?;
    peer_node.gossip_eth_block_to_peers(block_eth_payload.block())?;

    // Check that the genesis node rejected the block
    // The validation should fail because:
    // 1. The peer calculated PD base fee using base_fee_floor = 0.01
    // 2. The genesis node expects PD base fee calculated using base_fee_floor = 0.001
    // 3. The shadow transactions won't match, so the block is rejected
    let event_rx = genesis_node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash, event_rx).await;
    assert!(
        matches!(outcome, BlockValidationOutcome::Discarded(_)),
        "Genesis node should have rejected the block with incorrect PD base fee \
         (peer used base_fee_floor=0.01, genesis expects base_fee_floor=0.001), got: {:?}",
        outcome
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
