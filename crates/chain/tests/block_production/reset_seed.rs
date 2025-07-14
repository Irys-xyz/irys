use crate::utils::{AddTxError, IrysNodeTest};
use irys_actors::mempool_service::TxIngressError;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    irys::IrysSigner, BlockHash, ConsensusConfig, ConsensusOptions, IrysTransaction,
    IrysTransactionId, NodeConfig, NodeMode,
};
use std::collections::HashMap;
use tracing::{debug, error, warn};

#[actix_web::test]
async fn slow_heavy_reset_seeds_should_be_correctly_applied_by_the_miner_and_verified_by_the_peer(
) -> eyre::Result<()> {
    initialize_tracing();
    let max_seconds = 20;
    let approximate_steps_in_a_block = 12;
    let reset_interval_in_blocks = 4;
    // Approximately every 4 blocks - every 48 steps
    let reset_frequency = approximate_steps_in_a_block * reset_interval_in_blocks;
    let required_index_blocks_height: usize = reset_interval_in_blocks * 10;

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

    // retrieve block_migration_depth for use later
    let mut consensus = ctx_genesis_node.cfg.consensus.clone();
    let block_migration_depth: usize = consensus.get_mut().block_migration_depth.try_into()?;

    // the x2 is to ensure the block index of the peers contains the blocks we need
    // if this was left as block_migration_depth, because the index endpoint is used
    // on genesis node to sync to the peers, they lag behind by block_migration_depth
    let required_genesis_node_height: usize =
        required_index_blocks_height + (block_migration_depth * 2);

    // generate a txn and add it to the block...
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // mine x blocks on genesis
    ctx_genesis_node
        .mine_blocks(required_genesis_node_height)
        .await
        .expect("expected many mined blocks");
    // wait for block tree
    ctx_genesis_node
        .wait_until_height(required_genesis_node_height.try_into()?, max_seconds)
        .await?;
    // wait for block index
    ctx_genesis_node
        .wait_until_block_index_height(required_index_blocks_height.try_into()?, max_seconds)
        .await?;

    warn!(
        "Genesis node mined {} blocks, waiting for reset seed verification",
        required_genesis_node_height
    );

    let genesis_node_blocks = ctx_genesis_node
        .get_blocks(0, required_genesis_node_height as u64)
        .await
        .expect("expected to get mined blocks from genesis node");

    // 12 steps per block, 4 reset intervals, so 48 steps in total per reset interval
    let expected_reset_steps = [48, 96, 144, 192];
    let blocks_with_resets = genesis_node_blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let first_step = block.vdf_limiter_info.first_step_number();
            let last_step = block.vdf_limiter_info.global_step_number;
            let previous_block = if index == 0 {
                // The first block should not have a previous block
                None
            } else {
                Some(&genesis_node_blocks[index - 1])
            };
            let is_reset_block = expected_reset_steps
                .iter()
                .find(|reset_step| first_step <= **reset_step && last_step >= **reset_step);
            if is_reset_block.is_some() {
                Some((block.clone(), previous_block.cloned()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if blocks_with_resets.len() < 2 {
        error!(
            "Not enough blocks with reset seed found: {}, need to re-run the test",
            blocks_with_resets.len()
        );
        return Err(eyre::eyre!("No blocks with reset seed found"));
    }

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

    ctx_peer1_node
        .wait_until_height(required_index_blocks_height.try_into()?, max_seconds * 3)
        .await?;

    let peer_node_blocks = ctx_peer1_node
        .get_blocks(0, required_index_blocks_height as u64)
        .await
        .expect("expected peer 1 to be fully synced");

    warn!("Peer node synced, shutting down the nodes as we've got all blocks");

    ctx_genesis_node.stop().await;
    ctx_peer1_node.stop().await;

    for (index, peer_node_block) in peer_node_blocks.iter().enumerate() {
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
