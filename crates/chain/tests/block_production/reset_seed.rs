use crate::utils::{AddTxError, IrysNodeTest};
use irys_actors::mempool_service::TxIngressError;
use irys_chain::IrysNodeCtx;
use irys_types::{
    irys::IrysSigner, BlockHash, ConsensusConfig, ConsensusOptions, IrysBlockHeader,
    IrysTransaction, IrysTransactionId, NodeConfig, NodeMode,
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
#[test_log::test(actix_web::test)]
async fn slow_heavy_reset_seeds_should_be_correctly_applied_by_the_miner_and_verified_by_the_peer(
) -> eyre::Result<()> {
    let max_seconds = 20;
    let reset_frequency = 8; // Reset every 8 VDF steps
    let min_resets_required = 2; // Need at least 2 resets to verify behavior
    let block_migration_depth = 1;

    // Setting up parameters explicitly to check that the reset seed is applied correctly
    let mut consensus_config = ConsensusConfig::testnet();
    consensus_config.vdf.reset_frequency = reset_frequency;
    consensus_config.block_migration_depth = block_migration_depth;
    consensus_config.block_tree_depth = 200;

    // setup trusted peers connection data and configs for genesis and nodes
    let mut testnet_config_genesis = NodeConfig::testnet();
    testnet_config_genesis.consensus = ConsensusOptions::Custom(consensus_config);

    // setup trusted peers connection data and configs for genesis and nodes
    let account1 = testnet_config_genesis.signer();

    let ctx_genesis_node = IrysNodeTest::new_genesis(testnet_config_genesis.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;

    // Generate a test transaction to include in blocks
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // Mine blocks until we observe enough VDF resets
    let reset_frequency_u64 = reset_frequency as u64;
    let total_blocks_mined = ctx_genesis_node
        .mine_until_condition(
            |blocks| {
                blocks
                    .iter()
                    .filter(|block| {
                        block
                            .vdf_limiter_info
                            .reset_step(reset_frequency_u64)
                            .is_some()
                    })
                    .count()
                    >= min_resets_required
            },
            10,  // blocks_per_batch
            100, // max_blocks
            max_seconds,
        )
        .await?;

    let genesis_node_blocks = ctx_genesis_node
        .get_blocks(0, total_blocks_mined as u64)
        .await?;

    // Find and verify blocks that contain VDF resets
    let blocks_with_resets = ctx_genesis_node
        .get_blocks_with_vdf_resets(0, total_blocks_mined as u64)
        .await?;

    verify_reset_seeds(&blocks_with_resets, &genesis_node_blocks);

    // Verify seed continuity across all blocks
    verify_seed_continuity(&genesis_node_blocks, reset_frequency as u64);

    warn!("Reset seed verification completed, starting peer node to verify that syncing works");

    let mut ctx_peer1_node = ctx_genesis_node.testnet_peer();
    // Setting up mode to full validation sync to check that the reset seed is applied correctly
    //  and all blocks are validated successfully
    ctx_peer1_node.mode = NodeMode::PeerSync;
    let ctx_peer1_node = IrysNodeTest::new(ctx_peer1_node.clone())
        .start_with_name("PEER1")
        .await;
    ctx_peer1_node.start_public_api().await;

    // Wait for peer to sync
    let peer_sync_height = (total_blocks_mined as u64).saturating_sub(block_migration_depth as u64);
    ctx_peer1_node
        .wait_until_height(peer_sync_height, max_seconds * 3)
        .await?;

    warn!("Peer node synced, verifying blocks match");

    ctx_genesis_node
        .verify_blocks_match(&ctx_peer1_node, 0, peer_sync_height)
        .await?;

    warn!("Block verification completed, shutting down nodes");

    ctx_genesis_node.stop().await;
    ctx_peer1_node.stop().await;

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

/// Verify reset seeds are applied correctly
fn verify_reset_seeds(blocks_with_resets: &[IrysBlockHeader], all_blocks: &[IrysBlockHeader]) {
    for block in blocks_with_resets.iter() {
        // Find the previous block
        let previous_block = if block.height == 0 {
            None
        } else {
            all_blocks.iter().find(|b| b.height == block.height - 1)
        };

        // When performing a reset, the next reset seed is set to the block hash of the previous block,
        // and the current reset seed is set to the next reset seed of the previous block.
        let (expected_next_reset_seed, expected_current_reset_seed) = previous_block
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
}

/// Verify seed continuity across all blocks
fn verify_seed_continuity(blocks: &[IrysBlockHeader], reset_frequency: u64) {
    let mut resets_so_far = 0;

    for (index, block) in blocks.iter().enumerate() {
        let previous_block = if index == 0 {
            None
        } else {
            Some(&blocks[index - 1])
        };

        // Check that reset seeds are rotating correctly
        if let Some(prev_block) = previous_block {
            if block.vdf_limiter_info.reset_step(reset_frequency).is_some() {
                // For reset blocks:
                // - next_seed should always be the previous block's hash
                // - seed should be the previous block's next_seed (except for first reset)
                assert_eq!(block.vdf_limiter_info.next_seed, prev_block.block_hash);

                if resets_so_far == 0 {
                    // During the first reset, the seed should still be zero
                    assert_eq!(block.vdf_limiter_info.seed, BlockHash::zero());
                } else {
                    // After the first reset, seed should come from previous block's next_seed
                    assert_eq!(
                        block.vdf_limiter_info.seed,
                        prev_block.vdf_limiter_info.next_seed
                    );
                    // Verify that seeds are actually rotating (changing)
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
}
