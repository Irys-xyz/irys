use crate::utils::{AddTxError, IrysNodeTest};
use irys_actors::mempool_service::TxIngressError;
use irys_chain::IrysNodeCtx;
use irys_types::{
    irys::IrysSigner, BlockHash, ConsensusConfig, ConsensusOptions, IrysTransaction,
    IrysTransactionId, NodeConfig, NodeMode,
};
use std::collections::HashMap;
use tracing::{debug, warn};

/// This test verifies that VDF reset seeds work correctly. Reset seeds prevent miners
/// from precomputing VDF steps too far ahead by periodically changing the seed value.
///
/// The test:
/// 1. Mines blocks until we observe at least 2 VDF reset events
/// 2. For each reset, verifies that:
///    - next_seed = previous block's hash
///    - seed = previous block's next_seed
/// 3. Verifies non-reset blocks maintain seed continuity
/// 4. Spins up a peer node to verify reset seeds propagate correctly during sync
///
/// This approach is machine-independent since we detect resets dynamically rather
/// than trying to predict when they'll occur.
#[test_log::test(actix_web::test)]
async fn slow_heavy_reset_seeds_should_be_correctly_applied_by_the_miner_and_verified_by_the_peer(
) -> eyre::Result<()> {
    let max_seconds = 20;
    let reset_frequency = 8; // Reset every 8 VDF steps
    let min_resets_required = 2; // Need at least 2 resets to verify behavior

    // Setting up parameters explicitly to check that the reset seed is applied correctly
    let mut consensus_config = ConsensusConfig::testnet();
    consensus_config.vdf.reset_frequency = reset_frequency;
    consensus_config.block_migration_depth = 1;
    consensus_config.block_tree_depth = 200;

    // setup trusted peers connection data and configs for genesis and nodes
    let mut testnet_config_genesis = NodeConfig::testnet();
    testnet_config_genesis.consensus = ConsensusOptions::Custom(consensus_config);

    // setup trusted peers connection data and configs for genesis and nodes
    let account1 = testnet_config_genesis.signer();

    let ctx_genesis_node = IrysNodeTest::new_genesis(testnet_config_genesis.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;

    // generate a txn and add it to the block...
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // Mine blocks until we observe enough resets
    let mut resets_found = 0;
    let mut total_blocks_mined = 0;
    let blocks_per_batch = 10;
    let max_blocks = 100; // Safety limit
    
    let mut all_blocks = Vec::new();
    
    while resets_found < min_resets_required && total_blocks_mined < max_blocks {
        // Mine a batch of blocks
        ctx_genesis_node
            .mine_blocks(blocks_per_batch)
            .await
            .expect("expected to mine blocks");
        
        total_blocks_mined += blocks_per_batch;
        
        // Wait for blocks to be indexed
        ctx_genesis_node
            .wait_until_height(total_blocks_mined as u64, max_seconds)
            .await?;
        
        // Get all blocks mined so far
        let blocks = ctx_genesis_node
            .get_blocks(0, total_blocks_mined as u64)
            .await
            .expect("expected to get mined blocks");
        
        // Count resets in all blocks
        resets_found = blocks
            .iter()
            .filter(|block| block.vdf_limiter_info.reset_step(reset_frequency as u64).is_some())
            .count();
        
        all_blocks = blocks;
        
        debug!(
            "Mined {} blocks, found {} resets so far",
            total_blocks_mined, resets_found
        );
    }
    
    if resets_found < min_resets_required {
        return Err(eyre::eyre!(
            "Only found {} resets after mining {} blocks, needed at least {}",
            resets_found,
            total_blocks_mined,
            min_resets_required
        ));
    }
    
    warn!(
        "Genesis node mined {} blocks, found {} reset events",
        total_blocks_mined, resets_found
    );
    
    let genesis_node_blocks = all_blocks;

    // Debug: Log all blocks to understand VDF step progression
    for (index, block) in genesis_node_blocks.iter().enumerate() {
        let first_step = block.vdf_limiter_info.first_step_number();
        let last_step = block.vdf_limiter_info.global_step_number;
        debug!(
            "Block {} ({}): steps {} to {} (span: {})",
            index,
            block.block_hash,
            first_step,
            last_step,
            last_step - first_step + 1
        );
    }

    // Find all blocks that contain a reset
    let blocks_with_resets = genesis_node_blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            if block.vdf_limiter_info.reset_step(reset_frequency as u64).is_some() {
                let previous_block = if index == 0 {
                    None
                } else {
                    Some(&genesis_node_blocks[index - 1])
                };
                Some((block.clone(), previous_block.cloned()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for (block, previous_block) in blocks_with_resets.iter() {
        // When performing a reset, the next reset seed is set to the block hash of the previous block,
        // and the current reset seed is set to the next reset seed of the previous block.
        let (expected_next_reset_seed, expected_current_reset_seed) = previous_block
            .as_ref()
            .map(|block| (block.block_hash, block.vdf_limiter_info.next_seed))
            .unwrap_or_else(|| (BlockHash::zero(), BlockHash::zero()));

        let first_step = block.vdf_limiter_info.first_step_number();
        let last_step = block.vdf_limiter_info.global_step_number;

        assert_eq!(
            block.vdf_limiter_info.next_seed, expected_next_reset_seed,
            "Reset seed mismatch for block {:?}: expected {:?}, got {:?}",
            block.block_hash, expected_next_reset_seed, block.vdf_limiter_info.next_seed
        );
        assert_eq!(
            block.vdf_limiter_info.seed, expected_current_reset_seed,
            "Current reset seed mismatch for block {:?}: expected {:?}, got {:?}",
            block.block_hash, expected_current_reset_seed, block.vdf_limiter_info.seed
        );
        debug!(
            "Block with reset seed found: {}: first_step: {}, last_step: {}, next_seed: {:?}",
            block.block_hash, first_step, last_step, block.vdf_limiter_info.next_seed
        );
    }

    let mut resets_so_far = 0;
    // The test above verifies that the reset seed is applied correctly at a correct block,
    // this test ensures that the reset seed is carried over correctly on the blocks that don't
    // have a reset step
    for (index, block) in genesis_node_blocks.iter().enumerate() {
        let previous_block = if index == 0 {
            // The first block should not have a previous block
            None
        } else {
            Some(&genesis_node_blocks[index - 1])
        };

        // Check that reset seeds are rotating correctly
        if let Some(prev_block) = previous_block {
            if block
                .vdf_limiter_info
                .reset_step(reset_frequency as u64)
                .is_some()
            {
                if resets_so_far == 0 {
                    // During the first reset, both seeds should be zero
                    assert_eq!(block.vdf_limiter_info.seed, BlockHash::zero());
                    assert_eq!(block.vdf_limiter_info.next_seed, prev_block.block_hash);
                } else {
                    // After that point seeds should rotate each reset block
                    assert_eq!(
                        block.vdf_limiter_info.seed,
                        prev_block.vdf_limiter_info.next_seed
                    );
                    assert_ne!(
                        block.vdf_limiter_info.seed,
                        prev_block.vdf_limiter_info.seed
                    );
                    assert_ne!(
                        block.vdf_limiter_info.next_seed,
                        prev_block.vdf_limiter_info.next_seed
                    );
                }
                resets_so_far += 1;
            } else {
                assert_eq!(
                    block.vdf_limiter_info.seed, prev_block.vdf_limiter_info.seed,
                    "Seed should not change for non-reset blocks at index {}",
                    index
                );
            }
        } else {
            // The genesis block should not have a reset seed
            assert_eq!(block.vdf_limiter_info.seed, BlockHash::zero());
            assert_eq!(block.vdf_limiter_info.next_seed, BlockHash::zero());
        }
    }

    warn!("Reset seed verification completed, starting peer node to verify that syncing works");

    let mut ctx_peer1_node = ctx_genesis_node.testnet_peer();
    // Setting up mode to full validation sync to check that the reset seed is applied correctly
    //  and all blocks are validated successfully
    ctx_peer1_node.mode = NodeMode::PeerSync;
    let ctx_peer1_node = IrysNodeTest::new(ctx_peer1_node.clone())
        .start_with_name("PEER1")
        .await;
    ctx_peer1_node.start_public_api().await;

    // Wait for peer to sync all the blocks we mined
    // We need to wait for a bit less than total_blocks_mined due to block_migration_depth
    let peer_sync_height = (total_blocks_mined as u64).saturating_sub(2);
    ctx_peer1_node
        .wait_until_height(peer_sync_height, max_seconds * 3)
        .await?;

    let peer_node_blocks = ctx_peer1_node
        .get_blocks(0, peer_sync_height)
        .await
        .expect("expected peer 1 to be fully synced");

    warn!("Peer node synced, shutting down the nodes as we've got all blocks");

    ctx_genesis_node.stop().await;
    ctx_peer1_node.stop().await;

    // Compare only the blocks that the peer has synced
    let blocks_to_compare = peer_node_blocks.len();
    for index in 0..blocks_to_compare {
        let peer_node_block = &peer_node_blocks[index];
        let genesis_node_block = &genesis_node_blocks[index];
        assert_eq!(
            peer_node_block.block_hash, genesis_node_block.block_hash,
            "Block hash mismatch at index {}: expected {:?}, got {:?}",
            index, genesis_node_block.block_hash, peer_node_block.block_hash
        );
        assert_eq!(
            peer_node_block.vdf_limiter_info.next_seed,
            genesis_node_block.vdf_limiter_info.next_seed,
            "Next reset seed mismatch at index {}: expected {:?}, got {:?}",
            index,
            genesis_node_block.vdf_limiter_info.next_seed,
            peer_node_block.vdf_limiter_info.next_seed
        );
        assert_eq!(
            peer_node_block.vdf_limiter_info.seed, genesis_node_block.vdf_limiter_info.seed,
            "Current reset seed mismatch at index {}: expected {:?}, got {:?}",
            index, genesis_node_block.vdf_limiter_info.seed, peer_node_block.vdf_limiter_info.seed
        );
    }

    warn!("Reset seed peer verification completed");

    Ok(())
}

/// generate a test transaction, submit it to be added to mempool, return txn hashmap
async fn generate_test_transaction_and_add_to_block(
    node: &IrysNodeTest<IrysNodeCtx>,
    account: &IrysSigner,
) -> HashMap<IrysTransactionId, irys_types::IrysTransaction> {
    let data_bytes = "Test transaction!".as_bytes().to_vec();
    let mut irys_txs: HashMap<IrysTransactionId, IrysTransaction> = HashMap::new();
    match node.create_submit_data_tx(account, data_bytes).await {
        Ok(tx) => {
            irys_txs.insert(tx.header.id, tx);
        }
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            panic!("unfunded account error")
        }
        Err(AddTxError::TxIngress(TxIngressError::Skipped)) => {}
        Err(e) => panic!("unexpected error {:?}", e),
    }
    irys_txs
}
