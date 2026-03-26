use crate::utils::{read_block_from_state, BlockValidationOutcome, IrysNodeTest};
use irys_types::NodeConfig;

#[test_log::test(actix_web::test)]
async fn heavy_test_reth_block_with_pd_too_large_gets_rejected() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    // All 80 PD chunks must fit in a single partition so storage modules can store them
    // (default num_chunks_in_partition=10 would span 8 partitions).
    genesis_config.consensus.get_mut().num_chunks_in_partition = 100;
    let genesis_max_accepted_chunks_per_block = 10;
    let peer_max_accepted_chunks_per_block = 100;
    genesis_config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing")
        .max_pd_chunks_per_block = genesis_max_accepted_chunks_per_block;

    // Create and fund a signer for PD transaction
    let pd_tx_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // Start genesis node (Node A)
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Create peer node (Node B) — needs storage modules for chunk data
    let peer_node = {
        let peer_node = genesis_node
            .testing_peer_with_assignments(&pd_tx_signer)
            .await?;
        let mut peer_node = peer_node.stop().await;
        peer_node
            .cfg
            .consensus
            .get_mut()
            .hardforks
            .sprite
            .as_mut()
            .expect("Sprite hardfork must be configured for testing")
            .max_pd_chunks_per_block = peer_max_accepted_chunks_per_block;
        peer_node
            .start_and_wait_for_packing("peer", seconds_to_wait)
            .await
    };

    // === Phase 1: Upload 80 chunks of real data on GENESIS node ===
    // Data is uploaded to genesis (which has partition 0 in its storage modules).
    // The peer's PdService will P2P-fetch chunks from genesis when provisioning.
    let chunk_size = 32_usize;
    let num_pd_chunks: u64 = 80;
    let data = vec![0xEF_u8; num_pd_chunks as usize * chunk_size]; // 80 × 32 = 2560 bytes
    let data_start_offset = genesis_node
        .upload_data_for_pd(&pd_tx_signer, &data, 60)
        .await?;

    // Wait for peer to sync the blocks mined during upload/migration
    let genesis_height = genesis_node.get_canonical_chain_height().await;
    peer_node
        .wait_for_block_at_height(genesis_height, 30)
        .await?;

    // === Phase 2: Inject PD tx on peer, mine on peer, gossip to genesis ===
    // Fees must be high enough to meet min_pd_transaction_cost
    let tx_hash = peer_node
        .inject_pd_tx_at_real_offsets(
            &pd_tx_signer,
            data_start_offset,
            num_pd_chunks,
            1_000_000_000_000_000_u64, // 1e15 wei priority fee
            1_000_000_000_000_000_u64, // 1e15 wei base fee cap
            0,                         // nonce
        )
        .await?;

    // Wait for PdService to provision all chunks and mark tx ready
    peer_node.wait_for_ready_pd_tx(&tx_hash, 30).await?;

    // Verify chunk count exceeds genesis limit but fits peer limit
    assert!(
        num_pd_chunks > genesis_max_accepted_chunks_per_block,
        "expect to have more chunks than the genesis node would accept"
    );
    assert!(
        num_pd_chunks <= peer_max_accepted_chunks_per_block,
        "expect chunks to fit in the limits of the peer node"
    );

    // Mine block on peer containing the PD transaction
    let (block, block_eth_payload, _) = peer_node.mine_block_without_gossip().await?;

    // Verify the PD tx is in the block
    let pd_tx_included = block_eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|x| *x.tx_hash() == tx_hash);
    assert!(pd_tx_included, "expect the large pd tx to be included");

    // Gossip block to genesis node
    peer_node.gossip_block_to_peers(&block)?;
    peer_node.gossip_eth_block_to_peers(block_eth_payload.block())?;

    // Check that genesis node rejected the block (80 chunks > limit of 10)
    let event_rx = genesis_node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();
    let outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash, event_rx).await;
    assert!(
        matches!(outcome, BlockValidationOutcome::Discarded(_)),
        "Genesis node should have rejected the block containing 80 PD chunks (exceeds limit of 10), got: {:?}",
        outcome
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
