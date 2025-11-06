use crate::utils::{
    craft_data_poa_solution_from_tx, submit_solution_to_block_producer, IrysNodeTest,
};
use irys_actors::BlockProducerCommand;
use irys_chain::IrysNodeCtx;
use irys_domain::EpochSnapshot;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig};
use std::fs;
use std::sync::Arc;

/// End-to-end: mine to epoch boundary, craft a data PoA solution referencing a real mempool tx,
/// submit it to the block producer, and assert the boundary block is accepted and canonical.
///
/// Rationale:
/// - This exercises the full validation path, including PoA validation against the parent epoch
///   snapshot at the boundary.
/// - We set submit_ledger_epoch_length = 1 to encourage slot changes at the boundary, which makes
///   correctness dependent on using the PARENT snapshot for PoA validation.
/// - The test constructs a SolutionContext using a real tx (with tx_path/data_path), intended to
///   produce a data PoA block (as opposed to a capacity PoA block).
#[test_log::test(actix_web::test)]
async fn data_poa_boundary_acceptance() -> eyre::Result<()> {
    let max_seconds = 10;
    // Small configuration to make epochs quick and slot changes frequent.
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 1024;
    const BLOCKS_PER_EPOCH: u64 = 3;

    // Configure node
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 8;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.num_capacity_partitions = Some(8);
    // Encourage slot changes/expirations at every other epoch
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 2;
    // Make the initial difficulty permissive without blowing up scanning loops
    config.consensus.get_mut().difficulty_adjustment.block_time = 1;
    // Keep recall range small so test solvers don’t scan forever
    config.consensus.get_mut().num_chunks_in_recall_range = 1; // Default testing value is small
                                                               // speed up VDF step availability to reduce waiting
    config.consensus.get_mut().vdf.sha_1s_difficulty = 10_000;

    // Fund a user to post data
    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&user_signer]);

    // Start node and wait for packing
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("data_poa_boundary_acceptance", 30)
        .await;

    // Anchor is genesis
    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Debug: dump Submit and Publish slots and their partitions
    println!("SLOTS START at genesis");
    debug_partitions(get_snapshot(&node).await?);
    println!("SLOTS STOP at genesis");

    //check snapshot state is as expected at genesis
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _block1 = node.mine_block().await?;
    node.wait_until_height(1, max_seconds).await?;

    //check snapshot state is as expected at block 1, epoch 1
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    // Stake and pledge user so capacity partitions exist and backfill occurs
    let _ = node.post_stake_commitment_with_signer(&user_signer).await?;
    let _ = node.post_pledge_commitment_with_signer(&user_signer).await;
    node.mine_blocks(2).await?;
    node.wait_until_height(3, max_seconds).await?;

    //check snapshot state is as expected
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    let _ = node.mine_until_next_epoch().await?;
    let _ = node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;

    //check snapshot state is as expected
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    // Post a data tx and wait for mempool
    let tx = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;

    // Optional: stimulate ledger growth and observe multiple submit slots
    let _ = node
        .post_data_tx(anchor, vec![9_u8; DATA_SIZE], &user_signer)
        .await;
    let _ = node.mine_block().await?;

    node.wait_for_packing(20).await;

    //check snapshot state is as expected
    debug_asserts_first_slots(&node, 1, 1, 1, 1).await?;

    // Trigger slot expiration (submit_ledger_epoch_length = 1)
    let _ = node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;

    //check snapshot state is as expected
    // i.e. 11 submit slots with 3 partitions
    debug_asserts_all_slots(&node, 11, 3, 1, 1).await?;

    // Mine to the epoch boundary (end of current epoch)
    node.mine_until_next_epoch().await?;
    node.wait_for_packing(20).await;

    // Delete storage modules from disk to exercise recovery
    let storage_modules_dir = node
        .node_ctx
        .config
        .node_config
        .base_directory
        .join("storage_modules");
    if storage_modules_dir.exists() {
        let _ = fs::remove_dir_all(&storage_modules_dir);
    }

    // Fetch parent (epoch) block and its epoch snapshot;
    // choose a partition hash assigned to the Submit ledger to craft a data PoA
    let parent_snapshot = get_snapshot(&node).await?;

    // Debug: dump Submit and Publish slots and their partitions
    debug_partitions(parent_snapshot.clone());

    // Select a partition hash from the active or earlier non-empty Submit slot window.
    // This avoids selecting a future window whose slot_start is after prev_total_chunks.
    let partition_hash = {
        let cpp = node.node_ctx.config.consensus.num_chunks_in_partition;
        let npps = node.node_ctx.config.consensus.num_partitions_per_slot;
        let parent_block = node
            .get_block_by_height(node.get_canonical_chain_height().await)
            .await?;
        let prev_total_chunks = parent_block.data_ledgers[DataLedger::Submit].total_chunks;
        let slot_size = npps * cpp;
        let active_idx = ((prev_total_chunks.saturating_sub(1)) / slot_size) as usize;

        let submit_slots = parent_snapshot.ledgers.get_slots(DataLedger::Submit);

        // Try active slot, otherwise walk backwards to the nearest non-empty slot
        submit_slots
            .get(active_idx)
            .and_then(|slot| slot.partitions.first().copied())
            .or_else(|| {
                let slice = submit_slots.get(0..=active_idx)?;
                slice
                    .iter()
                    .rev()
                    .find_map(|slot| slot.partitions.first().copied())
            })
            .or_else(|| {
                // Final fallback: any non-empty slot (defensive)
                submit_slots
                    .iter()
                    .find_map(|slot| slot.partitions.first().copied())
            })
            .expect("Submit ledger should have at least one non-empty slot with a partition")
    };

    println!("partition_hash: {:?}", partition_hash);
    println!("tx: {:?}", &tx.header);

    // Craft a data PoA solution referencing the mempool tx
    let miner_addr = node.node_ctx.config.node_config.reward_address;

    // Debug: verify selected partition slot alignment
    {
        let cpp = node.node_ctx.config.consensus.num_chunks_in_partition;
        let npps = node.node_ctx.config.consensus.num_partitions_per_slot;
        let parent_block = node
            .get_block_by_height(node.get_canonical_chain_height().await)
            .await?;
        let prev_total_chunks = parent_block.data_ledgers[DataLedger::Submit].total_chunks;
        let slot_size = npps * cpp;
        let active_idx = ((prev_total_chunks.saturating_sub(1)) / slot_size) as usize;

        let pa = parent_snapshot
            .partition_assignments
            .get_assignment(partition_hash)
            .expect("partition assignment must exist");
        let slot_index = pa
            .slot_index
            .expect("slot_index must exist for data partition") as u64;
        let slot_start = slot_index * npps * cpp;

        println!(
            "DEBUG: chosen partition {:?}, slot_index={}, slot_start={}, prev_total_chunks={}, active_idx={}",
            partition_hash,
            slot_index,
            slot_start,
            prev_total_chunks,
            active_idx
        );
        if (slot_index as usize) > active_idx {
            println!(
                "DEBUG: WARNING: selected slot_index {} is in the future relative to active_idx {}",
                slot_index, active_idx
            );
        }
    }

    // Attempt to craft a solution that meets parent difficulty by retrying across VDF steps
    let solution = {
        let max_attempts = 200_usize;
        let mut attempt = 0_usize;
        loop {
            let sol =
                craft_data_poa_solution_from_tx(&node, &tx, partition_hash, miner_addr).await?;

            let parent_block = node
                .get_block_by_height(node.get_canonical_chain_height().await)
                .await?;
            let parent_diff = parent_block.diff;
            let solution_diff = irys_types::u256_from_le_bytes(&sol.solution_hash.0);

            if solution_diff >= parent_diff {
                break sol;
            }

            attempt += 1;
            if attempt >= max_attempts {
                eyre::bail!(
                    "Failed to craft solution meeting difficulty after {} attempts (last got={}, expected>={})",
                    attempt,
                    solution_diff,
                    parent_diff
                );
            }

            // Wait briefly for the VDF to advance, then try again
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };

    node.wait_until_height(15, 20).await?;

    tracing::error!("IGNORE ALL LOGS PRIOR TO THIS");

    // Ensure the block producer is not in test-limit mode
    node.node_ctx
        .service_senders
        .block_producer
        .send(BlockProducerCommand::SetTestBlocksRemaining(Some(1)))?;

    // Submit the solution to the block producer and get the built block
    let built_block = submit_solution_to_block_producer(&node, solution).await?;

    // Assert the built block is the first block of the new epoch and becomes canonical
    let canonical_hash = node.wait_until_height(built_block.height, 21).await?;
    //panic!("does not get here");
    assert_eq!(
        canonical_hash, built_block.block_hash,
        "Boundary block should become canonical at its height"
    );
    assert_eq!(
        built_block.height % BLOCKS_PER_EPOCH,
        1,
        "Boundary block should be the first block of the new epoch"
    );

    // Assert the produced block is a data PoA block (Submit ledger)
    assert_eq!(
        built_block.poa.ledger_id,
        Some(DataLedger::Submit as u32),
        "Built block PoA should target the Submit ledger"
    );

    // Assert the block includes the referenced tx in the Submit ledger
    let submit_ids = built_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .cloned()
        .unwrap_or_default();
    assert!(
        submit_ids.contains(&tx.header.id),
        "Built block should include the referenced tx in the Submit ledger"
    );

    Ok(())
}

async fn get_snapshot(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<Arc<EpochSnapshot>> {
    let parent_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    Ok(node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&parent_block.block_hash)
        .expect("parent epoch snapshot must exist at boundary"))
}

fn debug_partitions(parent_snapshot: Arc<EpochSnapshot>) {
    // Debug: dump Submit and Publish slots and their partitions
    {
        let submit_slots = parent_snapshot.ledgers.get_slots(DataLedger::Submit);
        let publish_slots = parent_snapshot.ledgers.get_slots(DataLedger::Publish);
        println!(
            "DEBUG: epoch_height={}, submit_slots={}, publish_slots={}",
            parent_snapshot.epoch_height,
            submit_slots.len(),
            publish_slots.len()
        );
        for (i, slot) in submit_slots.iter().enumerate() {
            println!(
                "DEBUG: Submit slot {}: partitions.len()={}, partitions={:?}",
                i,
                slot.partitions.len(),
                slot.partitions
            );
            for hash in &slot.partitions {
                if let Some(ass) = parent_snapshot
                    .partition_assignments
                    .data_partitions
                    .get(hash)
                {
                    println!(
                        "DEBUG:   partition {:?} -> ledger_id={:?}, slot_index={:?}, miner={:?}",
                        hash, ass.ledger_id, ass.slot_index, ass.miner_address
                    );
                }
            }
        }
        for (i, slot) in publish_slots.iter().enumerate() {
            println!(
                "DEBUG: Publish slot {}: partitions.len()={}, partitions={:?}",
                i,
                slot.partitions.len(),
                slot.partitions
            );
        }
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
    let snapshot = get_snapshot(node).await?;
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

// reusable debug asserts helper
async fn debug_asserts_all_slots(
    node: &IrysNodeTest<IrysNodeCtx>,
    expected_submit_slots: usize,
    expected_submit_partitions: usize,
    expected_publish_slots: usize,
    expected_publish_partitions: usize,
) -> eyre::Result<()> {
    let snapshot = get_snapshot(node).await?;
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
            .iter()
            .map(|s| s.partitions.len())
            .sum::<usize>(),
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
            .iter()
            .map(|s| s.partitions.len())
            .sum::<usize>(),
        "We expected {} publish partition(s)",
        expected_publish_partitions
    );
    Ok(())
}
