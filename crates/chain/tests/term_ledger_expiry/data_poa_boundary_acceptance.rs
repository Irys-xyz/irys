use crate::utils::{
    craft_data_poa_solution_from_tx, submit_solution_to_block_producer, IrysNodeTest,
};
use alloy_genesis::GenesisAccount;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, U256};

/// End-to-end: mine to epoch boundary, craft a data PoA solution referencing a real mempool tx,
/// submit it to the block producer, and assert the boundary block is accepted and canonical.
///
/// Rationale:
/// - This exercises the full validation path, including PoA validation against the parent epoch
///   snapshot at the boundary.
/// - We set submit_ledger_epoch_length = 1 to encourage slot changes at the boundary, which makes
///   correctness dependent on using the PARENT snapshot for PoA validation.
/// - The test constructs a SolutionContext using a real tx (with tx_path/data_path) so the block
///   producer builds a data PoA block, not a capacity PoA block.
#[test_log::test(actix_web::test)]
async fn data_poa_boundary_acceptance() -> eyre::Result<()> {
    // Small configuration to make epochs quick and slot changes frequent.
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 1024;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000; // 10 IRYS
    const BLOCKS_PER_EPOCH: u64 = 3;

    // Configure node
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 8;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    // Encourage slot changes/expirations at every epoch
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 1;

    // Fund a user to post data
    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    let user_address = user_signer.address();
    let initial_balance = U256::from(INITIAL_BALANCE);
    config.consensus.extend_genesis_accounts(vec![(
        user_address,
        GenesisAccount {
            balance: initial_balance.into(),
            ..Default::default()
        },
    )]);

    // Start node and wait for packing
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("data_poa_boundary_acceptance", 30)
        .await;

    // Anchor is genesis
    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post a publish data tx and wait for mempool
    let tx = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;

    // Mine to the epoch boundary (end of current epoch)
    node.mine_until_next_epoch().await?;

    // Fetch parent (epoch) block and its epoch snapshot;
    // choose a partition hash assigned to the Submit ledger to craft a data PoA
    let parent_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let parent_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&parent_block.block_hash)
        .expect("parent epoch snapshot must exist at boundary");

    // Pick the first partition hash from the first Submit slot
    let partition_hash = parent_snapshot
        .ledgers
        .get_slots(DataLedger::Submit)
        .first()
        .and_then(|slot| slot.partitions.first().copied())
        .expect("expected at least one Submit slot with a partition");

    // Craft a data PoA solution referencing the mempool tx
    let miner_addr = node.node_ctx.config.node_config.reward_address;
    let solution = craft_data_poa_solution_from_tx(&node, &tx, partition_hash, miner_addr).await?;

    // Submit the solution to the block producer and get the built block
    let built_block = submit_solution_to_block_producer(&node, solution).await?;

    // Assert the built block is the first block of the new epoch and becomes canonical
    let canonical_hash = node.wait_until_height(built_block.height, 20).await?;
    assert_eq!(
        canonical_hash, built_block.block_hash,
        "Boundary block should become canonical at its height"
    );
    assert_eq!(
        built_block.height % BLOCKS_PER_EPOCH,
        1,
        "Boundary block should be the first block of the new epoch"
    );

    Ok(())
}
