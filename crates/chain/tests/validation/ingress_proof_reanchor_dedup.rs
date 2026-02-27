use crate::{
    utils::{build_sealed_block, IrysNodeTest},
    validation::send_block_to_block_tree,
};
use irys_types::{DataLedger, NodeConfig};

/// Tests that re-anchored ingress proofs do not produce duplicate signer entries.
///
/// When the cache service re-anchors an expired ingress proof, the old entry in the DupSort
/// `IngressProofs` table must be deleted before the new one is stored. Without this,
/// both entries coexist (different anchor/signature bytes) and the block producer includes
/// both — triggering a `DuplicateIngressProofSigner` validation error.
///
/// This test verifies that we don't have local duplicates by:
/// 1. Mining enough blocks to expire the initial ingress proof anchor
/// 2. Having the peer node mine a block (which triggers re-anchoring)
/// 3. Asserting the peer's block contains exactly one ingress proof per signer
#[test_log::test(tokio::test)]
async fn slow_heavy3_reanchor_duplicate_ingress_proof_signers() -> eyre::Result<()> {
    let seconds_to_wait = 30;

    // Configure consensus for short epochs and low expiry depth.
    // number_of_ingress_proofs_total = 3 is critical: the test only has two distinct
    // signers (genesis + peer), so requiring 3 proofs means the threshold can only be
    // met if reanchored proofs are counted — verifying that reanchoring does not produce
    // duplicate signer entries that would inflate the count.
    let mut genesis_config = NodeConfig::testing()
        .with_consensus(|c| {
            c.chunk_size = 32;
            c.mempool.ingress_proof_anchor_expiry_depth = 3;
            c.block_migration_depth = 1;
            c.epoch.num_blocks_in_epoch = 3;
            c.hardforks.frontier.number_of_ingress_proofs_total = 3;
            c.hardforks.frontier.number_of_ingress_proofs_from_assignees = 0;
        })
        .with_genesis_peer_discovery_timeout(1000);

    // Create and fund peer signer
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;

    // Mine through 2 epochs for partition assignment
    genesis_node.mine_until_next_epoch().await?;
    genesis_node.mine_until_next_epoch().await?;

    let current_height = genesis_node.get_canonical_chain_height().await;
    peer_node
        .wait_for_block_at_height(current_height, seconds_to_wait)
        .await?;
    let genesis_signer = genesis_config.signer();

    let chunks = vec![[10_u8; 32], [20_u8; 32], [30_u8; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    let data_tx = genesis_node
        .create_signed_data_tx(&genesis_signer, data)
        .await?;
    let tx_id = data_tx.header.id;

    // Post tx to genesis and wait for it to reach peer
    genesis_node.post_data_tx_raw(&data_tx.header).await;
    peer_node.wait_for_mempool(tx_id, seconds_to_wait).await?;

    // Upload chunks to the peer node only
    peer_node.gossip_disable();
    peer_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    peer_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    peer_node.post_chunk_32b(&data_tx, 2, &chunks).await;

    let order = [
        // forward genesis node
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
    ];

    for (header, eth_block, block_txs) in order.iter() {
        // sync peer node up to date with genesis, this causes ingress proof reanchoring
        peer_node
            .node_ctx
            .block_pool
            .add_execution_payload_to_cache(eth_block.block().clone())
            .await;
        let sealed_block = build_sealed_block(header.as_ref().clone(), block_txs)?;
        send_block_to_block_tree(&peer_node.node_ctx, sealed_block, false).await?;
    }

    // Wait for peer to fully validate all 6 fed blocks (and trigger re-anchoring)
    // before re-enabling gossip
    peer_node
        .wait_for_block_at_height(order[5].0.height, seconds_to_wait)
        .await?;

    // Peer mines a block with the genesis ingress proof present in its db
    peer_node.gossip_enable();
    genesis_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 2, &chunks).await;
    peer_node
        .wait_for_multiple_ingress_proofs_no_mining(vec![data_tx.header.id], 2, seconds_to_wait)
        .await?;
    let peer_block = peer_node.mine_block().await?;

    // Assert the peer's block does not contain any proofs because it only has 2/3 ingress proofs
    let peer_publish_ledger = &peer_block.data_ledgers[DataLedger::Publish];
    let peer_proofs = peer_publish_ledger.proofs.as_ref();
    assert!(
        peer_proofs.is_none(),
        "Peer block should not have any proofs because required proof count is 3"
    );

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
