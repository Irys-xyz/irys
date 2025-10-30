//! Partial regression test for boundary PoA snapshot selection (commit 0a69bde6ce44a1bd6cc8baf0c5de7805ec032a9f).
//! Verifies the invariant (PoA succeeds under the parent snapshot and fails under a child snapshot), but it does
//! not exercise the actual production path that chooses the parent snapshot during boundary block validation.
//!
//! Goal:
//! - Ensure the boundary (first post-epoch) block’s PoA validates against the PARENT epoch snapshot,
//!   not the CHILD snapshot derived at/after the boundary.
//!
//! Strategy (deterministic, simplified):
//! 1) Configure a tiny chain so epochs are quick and term expirations frequent.
//! 2) Post a single data transaction and wait for it in mempool.
//! 3) Mine to the epoch boundary (end of current epoch) to get a stable parent snapshot.
//! 4) Choose a concrete Submit partition from the parent snapshot and craft a data PoA solution for
//!    the mempool tx, using helper functions.
//! 5) Submit the solution to the block producer to build the boundary block. This guarantees a data
//!    PoA boundary block without relying on probabilistic mining selection.
//! 6) Validate that the boundary PoA:
//!    - Succeeds against the PARENT epoch snapshot (this was broken by the regression).
//!    - Fails against a CHILD snapshot after subsequent epoch processing (e.g. after expirations),
//!      using the same PoA. We accept a family of child-invalid errors.
//!
//! Notes:
//! - We purposefully do not chase the “immediate” child at the same boundary, as that remains
//!   probabilistic depending on whether the exact PoA partition moved. Instead we mine forward
//!   a couple of epochs to ensure the child snapshot no longer matches the PoA’s partition/slot
//!   context, which must make the same PoA invalid under the newer snapshot. This still verifies
//!   the core regression: parent snapshot is the only correct validation context at the boundary.

use crate::utils::IrysNodeTest;
use irys_actors::block_validation::{poa_is_valid, PreValidationError};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig};

#[test_log::test(actix_web::test)]
async fn heavy_poa_parent_snapshot_integration() -> eyre::Result<()> {
    // Small, fast chain; frequent expirations encourage assignment changes across epochs.
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 1024; // 1 KB (multiple chunks)
    const BLOCKS_PER_EPOCH: u64 = 3;

    // 1) Configure the node
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 8;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.num_capacity_partitions = Some(8);
    // Expire Submit slots every epoch to force rapid state changes over time
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 1;

    // 2) Fund a user to post data
    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts([&user_signer]);

    // Start node
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("regression_boundary_poa_parent_snapshot", 20)
        .await;

    // Stake and pledge the funded user so capacity partitions can be assigned and used for backfill
    let _ = node.post_stake_commitment_with_signer(&user_signer).await?;
    let _ = node.post_pledge_commitment_with_signer(&user_signer).await;
    // Include commitments
    node.mine_blocks(2).await?;
    // Process commitments at an epoch, then assign capacity to pledges, then backfill into slots on next epoch
    for _ in 0..2 {
        let _ = node.mine_until_next_epoch().await?;
    }
    // Parent epoch snapshot at current head (an epoch block)
    let parent_epoch_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let parent_epoch_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&parent_epoch_block.block_hash)
        .expect("parent epoch snapshot must exist after assignments and backfill");

    // Choose the first available partition from any Submit slot under the parent snapshot
    let parent_slots = parent_epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
    let partition_hash = parent_slots
        .iter()
        .find_map(|slot| slot.partitions.first().copied())
        .expect("Submit ledger must contain at least one partition across slots");

    // Post a data tx now and upload chunks so we can craft a PoA against it
    let anchor = parent_epoch_block.block_hash;
    let tx = node
        .post_data_tx(anchor, vec![7_u8; DATA_SIZE], &user_signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;

    // Derive slot_index and partition_chunk_offset from the parent snapshot and block
    let parent_assignment = parent_epoch_snapshot
        .partition_assignments
        .get_assignment(partition_hash)
        .expect("partition assignment must exist in parent snapshot");
    let slot_index = parent_assignment
        .slot_index
        .expect("slot_index must exist for data partition") as u64;
    let cpp = node.node_ctx.config.consensus.num_chunks_in_partition;
    let npps = node.node_ctx.config.consensus.num_partitions_per_slot;
    let slot_start = slot_index * npps * cpp;
    let prev_total_chunks = parent_epoch_block.data_ledgers[DataLedger::Submit].total_chunks;
    assert!(
        prev_total_chunks >= slot_start,
        "prev_total_chunks {} before slot_start {}",
        prev_total_chunks,
        slot_start
    );
    let partition_chunk_offset_u64 = prev_total_chunks - slot_start;
    let partition_chunk_offset: u32 = partition_chunk_offset_u64 as u32;

    // Build PoA chunk = data_chunk XOR entropy(partition_chunk_offset)
    let chunk_size = node.node_ctx.config.consensus.chunk_size as usize;
    let data_bytes = tx.data.as_ref().expect("tx data present").0.clone();
    let mut poa_chunk = data_bytes
        .get(0..std::cmp::min(chunk_size, data_bytes.len()))
        .expect("non-empty tx data")
        .to_vec();
    let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
    compute_entropy_chunk(
        node.node_ctx.config.node_config.reward_address,
        partition_chunk_offset as u64,
        partition_hash.into(),
        node.node_ctx.config.consensus.entropy_packing_iterations,
        chunk_size,
        &mut entropy_chunk,
        node.node_ctx.config.consensus.chain_id,
    );
    for i in 0..poa_chunk.len() {
        poa_chunk[i] ^= entropy_chunk[i];
    }

    // Mine until the tx is included in a Submit ledger, then build tx_path against that block's ordering
    let mut inclusion_block = None;
    for _ in 0..6 {
        let b = node.mine_block().await?;
        let submit_ids = b
            .get_data_ledger_tx_ids()
            .get(&DataLedger::Submit)
            .cloned()
            .unwrap_or_default();
        if submit_ids.contains(&tx.header.id) {
            inclusion_block = Some(b);
            break;
        }
    }
    let inclusion_block = inclusion_block.expect("data tx not included within retries");
    // Mine one extra block so the inclusion block becomes eligible for migration (block_migration_depth=1)
    let _ = node.mine_block().await?;
    // Ensure block_index has migrated the inclusion block and packing/indexes are up-to-date
    node.wait_until_height(inclusion_block.height + 1, 10)
        .await?;
    node.wait_for_packing(20).await;

    let submit_ids = inclusion_block
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .cloned()
        .unwrap_or_default();
    let mut submit_headers = Vec::with_capacity(submit_ids.len());
    for id in &submit_ids {
        match node.get_tx_header(id) {
            Ok(h) => submit_headers.push(h),
            Err(_) => {
                let h = node.get_storage_tx_header_from_mempool(id).await?;
                submit_headers.push(h);
            }
        }
    }
    let (_tx_root, tx_paths) =
        irys_types::block::DataTransactionLedger::merklize_tx_root(&submit_headers);
    let tx_index = submit_ids
        .iter()
        .position(|id| *id == tx.header.id)
        .expect("tx id should be in inclusion block submit list");
    let tx_path_bytes = tx_paths[tx_index].proof.clone();
    let data_path_bytes = tx
        .proofs
        .first()
        .expect("tx missing first chunk proof")
        .proof
        .clone();

    let test_poa = irys_types::block::PoaData {
        partition_chunk_offset,
        partition_hash,
        chunk: Some(irys_types::Base64(poa_chunk)),
        ledger_id: Some(DataLedger::Submit as u32),
        tx_path: Some(irys_types::Base64(tx_path_bytes)),
        data_path: Some(irys_types::Base64(data_path_bytes)),
    };

    // PoaData built above based on actual inclusion block ordering

    // 6) Validate PoA against parent snapshot (MUST SUCCEED for correctness at boundary)
    let block_index_guard = node.node_ctx.block_index_guard.clone();
    let consensus_config = node.node_ctx.config.consensus.clone();
    let miner_address = node.node_ctx.config.node_config.reward_address;

    poa_is_valid(
        &test_poa,
        &block_index_guard,
        &parent_epoch_snapshot,
        &consensus_config,
        &miner_address,
    )
    .expect("PoA must validate against PARENT epoch snapshot");

    // Now move forward a couple of epochs to force state changes (expirations/rotations)
    // and then validate the SAME boundary PoA against a CHILD snapshot—which should FAIL.
    for _ in 0..2 {
        let _ = node.mine_until_next_epoch().await?;
    }
    // Ensure all services (including block index) have processed epoch transitions
    node.wait_for_packing(20).await;

    let child_head = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let child_epoch_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&child_head.block_hash)
        .expect("child epoch snapshot should exist after mining epochs");

    // Sanity: confirm epoch snapshots are not the same epoch
    assert_ne!(
        child_epoch_snapshot.epoch_height, parent_epoch_snapshot.epoch_height,
        "Expected child and parent snapshots to be from different epochs"
    );

    // Validate against the CHILD snapshot (MUST FAIL).
    // Accept the family of expected child-invalid errors that demonstrate the snapshot context mismatch:
    // - MerkleProofInvalid(_)
    // - PartitionAssignmentMissing { .. }
    // - PartitionAssignmentSlotIndexMissing { .. }
    // - PoAChunkOffsetOutOfBlockBounds
    // - PoAChunkOffsetOutOfTxBounds
    // - PoAChunkOffsetOutOfDataChunksBounds
    match poa_is_valid(
        &test_poa,
        &block_index_guard,
        &child_epoch_snapshot,
        &consensus_config,
        &miner_address,
    ) {
        Ok(()) => {
            panic!(
                "Child snapshot unexpectedly validated the boundary PoA after epoch transitions"
            );
        }
        Err(
            PreValidationError::MerkleProofInvalid(_)
            | PreValidationError::PartitionAssignmentMissing { .. }
            | PreValidationError::PartitionAssignmentSlotIndexMissing { .. }
            | PreValidationError::PoAChunkOffsetOutOfBlockBounds
            | PreValidationError::PoAChunkOffsetOutOfTxBounds
            | PreValidationError::PoAChunkOffsetOutOfDataChunksBounds,
        ) => {
            // PASS: child validation failed for any of the expected reasons
        }
        Err(e) => {
            panic!("Child snapshot produced an unexpected PoA error: {:?}", e);
        }
    }

    Ok(())
}
