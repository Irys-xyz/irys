use std::{sync::Arc, time::Duration};

use crate::{utils::IrysNodeTest, validation::send_block_to_block_tree};
use irys_database::{db::IrysDatabaseExt as _, tables::IrysBlockHeaders};
use irys_types::{irys::IrysSigner, CommitmentTransaction, NodeConfig};
use reth_db::{transaction::DbTxMut as _, Database as _};

async fn stake_and_pledge_signer(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    pledge_count: usize,
) -> eyre::Result<CommitmentTransaction> {
    let commitment_tx = node.post_stake_commitment_with_signer(signer).await?;
    for _ in 0..pledge_count {
        node.post_pledge_commitment_with_signer(signer).await;
    }
    Ok(commitment_tx)
}

/// Reproduces the DuplicateIngressProofSigner bug triggered by re-anchoring.
///
/// When the cache service re-anchors an expired ingress proof, `reanchor_and_store_ingress_proof`
/// stores the new proof via `put` into the DupSort `IngressProofs` table without deleting the old
/// one. Both entries coexist because they have different anchor/signature bytes. On a lagging node,
/// both proofs have valid anchors, so the mempool includes both in a block — triggering the
/// DuplicateIngressProofSigner validation error.
#[test_log::test(tokio::test)]
async fn heavy_reanchor_duplicate_ingress_proof_signers() -> eyre::Result<()> {
    let seconds_to_wait = 30;

    // Configure consensus for short epochs and low expiry depth
    let mut genesis_config = NodeConfig::testing()
        .with_consensus(|c| {
            c.chunk_size = 32;
            c.mempool.ingress_proof_anchor_expiry_depth = 3;
            c.block_migration_depth = 1;
            c.epoch.num_blocks_in_epoch = 3;
            // c.num_partitions_per_slot = 3;
            // Need >=2 total proofs so mempool includes multiple from same signer
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

    // Start peer node

    // Mine through 2 epochs for partition assignment
    genesis_node.mine_until_next_epoch().await?;
    genesis_node.mine_until_next_epoch().await?;

    let current_height = genesis_node.get_canonical_chain_height().await;
    peer_node
        .wait_until_height(current_height, seconds_to_wait)
        .await?;
    let genesis_signer = genesis_config.signer();

    // === Phase 1: Generate initial proof ===

    // Post data tx (3 chunks x 32 bytes)
    let chunks = vec![[10_u8; 32], [20_u8; 32], [30_u8; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    let data_tx = genesis_node
        .create_signed_data_tx(&genesis_signer, data)
        .await?;
    let tx_id = data_tx.header.id;
    let data_root = data_tx.header.data_root;

    // Post tx to genesis and wait for it to reach peer
    genesis_node.post_data_tx_raw(&data_tx.header).await;
    peer_node.wait_for_mempool(tx_id, seconds_to_wait).await?;

    // Upload all 3 chunks to genesis
    peer_node.gossip_disable();
    genesis_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 2, &chunks).await;
    peer_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    peer_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    peer_node.post_chunk_32b(&data_tx, 2, &chunks).await;

    let order = [
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
        &genesis_node.mine_block_without_gossip().await?,
    ];
    // todo assert that theres an ingress proof from genesis node in order[0]
    for (block, eth_block, block_txs) in order.iter() {
        tracing::error!(block_heght = block.height, ?block.cumulative_diff, "block");
        // IMPORTANT: Add execution payload to cache BEFORE sending block header
        // This prevents a race condition where validation starts before the payload is available
        peer_node
            .node_ctx
            .block_pool
            .add_execution_payload_to_cache(eth_block.block().clone())
            .await;
        send_block_to_block_tree(
            &peer_node.node_ctx,
            block.clone(),
            block_txs.clone(),
            false,
        )
        .await?;
    }
    tracing::error!("here before mining");
    tracing::error!("here before mining");
    tracing::error!("here before mining");
    tracing::error!("here before mining");
    let block3 = peer_node.mine_block().await?;
    // todo assert that theres an ingress proof from peer node
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Cleanup
    peer_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
