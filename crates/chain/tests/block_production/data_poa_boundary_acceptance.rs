use crate::utils::{
    craft_data_poa_solution_from_tx, get_epoch_snapshot, read_block_from_state,
    submit_solution_to_block_producer, BlockValidationOutcome, IrysNodeTest,
};
use irys_actors::BlockProducerCommand;
use irys_chain::IrysNodeCtx;
use irys_domain::EpochSnapshot;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig};
use std::sync::Arc;

/// End-to-end: mine to epoch boundary, craft a data PoA solution referencing existing ledger data,
/// submit it to the block producer, and assert the boundary block is accepted and canonical.
///
/// Rationale:
/// - This exercises the full validation path, including PoA validation against the parent epoch
///   snapshot at the boundary.
/// - We set submit_ledger_epoch_length = 2 to encourage slot changes at the boundary, which makes
///   correctness dependent on using the PARENT snapshot for PoA validation.
/// - The test constructs a SolutionContext using a real tx's data (with tx_path/data_path) that
///   is already in the ledger, producing a data PoA block (as opposed to a capacity PoA block).
#[test_log::test(actix_web::test)]
async fn data_poa_boundary_acceptance() -> eyre::Result<()> {
    let max_seconds = 10;
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 256;
    const BLOCKS_PER_EPOCH: u64 = 3;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 8;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.num_capacity_partitions = Some(100);
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 2;
    config.consensus.get_mut().difficulty_adjustment.block_time = 1;
    config
        .consensus
        .get_mut()
        .difficulty_adjustment
        .difficulty_adjustment_interval = 1_000_000;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().vdf.sha_1s_difficulty = 10_000;

    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&user_signer]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("data_poa_boundary_acceptance", 30)
        .await;

    let slot_size = node.node_ctx.config.consensus.num_partitions_per_slot
        * node.node_ctx.config.consensus.num_chunks_in_partition;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    debug_partitions(get_epoch_snapshot(&node).await?);
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _block1 = node.mine_block().await?;
    node.wait_until_height(1, max_seconds).await?;

    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _ = node.post_stake_commitment_with_signer(&user_signer).await?;
    let _ = node.post_pledge_commitment_with_signer(&user_signer).await;
    node.mine_blocks(2).await?;
    node.wait_until_height(3, max_seconds).await?;
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _ = node.mine_until_next_epoch().await?;
    let _ = node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    // Post TWO txs so data extends into slot 1 (which should have partitions).
    let _tx1 = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    let _ = node.mine_block().await?;
    let tx = node
        .post_data_tx(anchor, vec![9_u8; DATA_SIZE], &user_signer)
        .await;
    let _ = node.mine_block().await?;
    // tx2 starts at slot 1
    let tx_ledger_offset: u64 = slot_size;

    node.wait_for_packing(20).await;
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _ = node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;
    let snapshot = get_epoch_snapshot(&node).await?;
    let submit_slots = snapshot.ledgers.get_slots(irys_types::DataLedger::Submit);
    assert!(
        !submit_slots.is_empty(),
        "Should have at least 1 submit slot after epochs"
    );
    let total_partitions: usize = submit_slots.iter().map(|s| s.partitions.len()).sum();
    assert!(
        total_partitions > 0,
        "Should have at least 1 partition across submit slots"
    );

    node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;

    node.node_ctx
        .service_senders
        .block_producer
        .send(BlockProducerCommand::SetTestBlocksRemaining(Some(0)))?;

    let parent_snapshot = get_epoch_snapshot(&node).await?;
    debug_partitions(parent_snapshot.clone());

    // Select partition assigned to the slot containing tx_ledger_offset
    let target_slot_idx = (tx_ledger_offset / slot_size) as usize;
    let partition_hash = {
        let submit_slots = parent_snapshot.ledgers.get_slots(DataLedger::Submit);
        submit_slots
            .iter()
            .flat_map(|slot| slot.partitions.iter().copied())
            .find(|&partition_hash| {
                parent_snapshot
                    .partition_assignments
                    .get_assignment(partition_hash)
                    .and_then(|pa| pa.slot_index)
                    .map(|si| si == target_slot_idx)
                    .unwrap_or(false)
            })
            .expect("Submit ledger should have a partition assigned to the slot containing the tx")
    };

    let miner_addr = node.node_ctx.config.node_config.reward_address;

    // Verify partition slot alignment
    {
        let pa = parent_snapshot
            .partition_assignments
            .get_assignment(partition_hash)
            .expect("partition assignment must exist");
        let slot_index = pa
            .slot_index
            .expect("slot_index must exist for data partition") as u64;
        assert_eq!(
            slot_index as usize, target_slot_idx,
            "partition slot_index should match the tx's slot"
        );
    }

    // Enable mining for VDF advancement while retrying solutions
    node.node_ctx.start_mining()?;

    let solution = {
        let max_attempts = 500_usize;
        let mut attempt = 0_usize;
        let delay_ms = 50_u64;
        let parent_block = node
            .get_block_by_height(node.get_canonical_chain_height().await)
            .await?;
        let parent_diff = parent_block.diff;

        loop {
            let sol_result = craft_data_poa_solution_from_tx(
                &node,
                &tx,
                partition_hash,
                miner_addr,
                Some(tx_ledger_offset),
            )
            .await;

            match sol_result {
                Ok(sol) => {
                    let solution_diff = irys_types::u256_from_le_bytes(&sol.solution_hash.0);
                    if solution_diff >= parent_diff {
                        tracing::debug!(attempt, "Found valid solution after attempts");
                        break sol;
                    }
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if !err_msg.contains("recall range mismatch") {
                        return Err(e);
                    }
                }
            }

            attempt += 1;
            if attempt >= max_attempts {
                eyre::bail!(
                    "Failed to craft solution after {} attempts (last error: recall range or difficulty mismatch)",
                    attempt
                );
            }

            // Short constant delay to allow VDF advancement between attempts
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    };

    node.node_ctx
        .service_senders
        .block_producer
        .send(BlockProducerCommand::SetTestBlocksRemaining(Some(1)))?;

    let built_block = submit_solution_to_block_producer(&node, solution).await?;
    let validation_outcome = read_block_from_state(&node.node_ctx, &built_block.block_hash).await;
    node.node_ctx.stop_mining()?;

    match validation_outcome {
        BlockValidationOutcome::StoredOnNode(chain_state) => {
            tracing::info!("Block validated with state: {:?}", chain_state);
        }
        BlockValidationOutcome::Discarded(error) => {
            return Err(eyre::eyre!("Block validation failed: {:?}", error));
        }
    }

    let canonical_hash = node.wait_until_height(built_block.height, 30).await?;
    assert_eq!(
        canonical_hash, built_block.block_hash,
        "Boundary block should become canonical at its height"
    );
    assert_eq!(
        built_block.height % BLOCKS_PER_EPOCH,
        1,
        "Boundary block should be the first block of the new epoch"
    );
    assert_eq!(
        built_block.poa.ledger_id,
        Some(DataLedger::Submit as u32),
        "Built block PoA should target the Submit ledger"
    );

    Ok(())
}

fn debug_partitions(parent_snapshot: Arc<EpochSnapshot>) {
    let submit_slots = parent_snapshot.ledgers.get_slots(DataLedger::Submit);
    let publish_slots = parent_snapshot.ledgers.get_slots(DataLedger::Publish);
    tracing::debug!(
        epoch_height = parent_snapshot.epoch_height,
        submit_slots = submit_slots.len(),
        publish_slots = publish_slots.len(),
        "Epoch snapshot partition info"
    );
    for (i, slot) in submit_slots.iter().enumerate() {
        tracing::debug!(
            slot_index = i,
            partitions_count = slot.partitions.len(),
            partitions = ?slot.partitions,
            "Submit slot"
        );
        for hash in &slot.partitions {
            if let Some(ass) = parent_snapshot
                .partition_assignments
                .data_partitions
                .get(hash)
            {
                tracing::debug!(
                    partition = ?hash,
                    ledger_id = ?ass.ledger_id,
                    slot_index = ?ass.slot_index,
                    miner = ?ass.miner_address,
                    "Partition assignment"
                );
            }
        }
    }
    for (i, slot) in publish_slots.iter().enumerate() {
        tracing::debug!(
            slot_index = i,
            partitions_count = slot.partitions.len(),
            partitions = ?slot.partitions,
            "Publish slot"
        );
    }
}

// reusable debug asserts helper
async fn debug_asserts_first_slots(
    node: &IrysNodeTest<IrysNodeCtx>,
    expected_submit_slots: usize,
    expected_submit_partitions: usize,
    expected_publish_slots: usize,
    expected_publish_partitions: usize,
) -> eyre::Result<()> {
    let snapshot = get_epoch_snapshot(node).await?;
    let submit_slots = snapshot.ledgers.get_slots(DataLedger::Submit);
    let publish_slots = snapshot.ledgers.get_slots(DataLedger::Publish);

    assert_eq!(
        expected_submit_slots,
        submit_slots.len(),
        "We expected {} Submit slot(s)",
        expected_submit_slots
    );
    assert_eq!(
        expected_submit_partitions,
        submit_slots
            .first()
            .map(|s| s.partitions.len())
            .unwrap_or_default(),
        "We expected {} Submit partition(s)",
        expected_submit_partitions
    );
    assert_eq!(
        expected_publish_slots,
        publish_slots.len(),
        "We expected {} publish slot(s)",
        expected_publish_slots
    );
    assert_eq!(
        expected_publish_partitions,
        publish_slots
            .first()
            .map(|s| s.partitions.len())
            .unwrap_or_default(),
        "We expected {} publish partition(s)",
        expected_publish_partitions
    );
    Ok(())
}
