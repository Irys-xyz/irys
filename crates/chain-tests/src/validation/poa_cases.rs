use crate::utils::IrysNodeTest;
use irys_actors::block_validation::poa_is_valid;
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    Base64, DataLedger, DataTransaction, DataTransactionLedger, LedgerChunkOffset, NodeConfig,
    PoaData,
};
use tracing::info;

//==============================================================================
// Proper PoA test
//==============================================================================
// Start a node
// Post enough transactions to add an additional submit ledger slot
// Mine an epoch block to get the submit ledger slots updated
// Create poa's for each of the chunks and validate them
// (this will validate chunks in the first and second submit ledger slot)
#[tokio::test]
async fn multi_slot_poa_test() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "info") };
    initialize_tracing();

    let seconds_to_wait = 20;
    let chunk_size: usize = 32;

    let node_config = NodeConfig::testing().with_consensus(|consensus| {
        consensus.chunk_size = chunk_size as u64;
        consensus.num_partitions_per_slot = 3;
        consensus.epoch.num_blocks_in_epoch = 2;
        consensus.num_chunks_in_partition = 6;
        consensus.num_chunks_in_recall_range = 2;
        consensus.entropy_packing_iterations = 1_000;
        consensus.block_migration_depth = 1;
    });

    let genesis_node = IrysNodeTest::new_genesis(node_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.stop_mining();
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();

    // Create a bunch of TX chunks
    let tx_data = vec![
        vec![[0; 32], [1; 32], [2; 32]], // tx0
        vec![[3; 32], [4; 32], [5; 32]], // tx1
        vec![[6; 32], [7; 32], [8; 32]], // tx2
    ];

    // Build a vec of signed data transactions from the chunks
    let mut txs: Vec<DataTransaction> = Vec::new();

    for tx_data_chunks in &tx_data {
        let mut data: Vec<u8> = Vec::new();
        for chunk in tx_data_chunks {
            data.extend_from_slice(chunk);
        }
        let tx = genesis_node
            .create_signed_data_tx(&genesis_signer, data)
            .await
            .expect("to create a signed data tx");
        txs.push(tx);
    }

    // Post the data transactions to fill the submit ledger
    genesis_node.post_data_tx_raw(&txs[0].header).await;
    genesis_node.post_data_tx_raw(&txs[1].header).await;
    genesis_node.post_data_tx_raw(&txs[2].header).await;

    // Mine a block to get the tx stored in the ledger
    let new_block = genesis_node.mine_block().await?;
    info!(
        "new_block:\n{:?}",
        new_block.data_ledgers[DataLedger::Submit]
    );

    let mut sorted_headers = Vec::new();
    let mut sorted_txs = Vec::new();
    for txid in &new_block.data_ledgers[DataLedger::Submit].tx_ids.0 {
        let tx = txs.iter().find(|tx| tx.header.id == *txid).unwrap().clone();
        sorted_headers.push(tx.header.clone());
        sorted_txs.push(tx);
    }

    // Validate the calculated tx_root matches the one in the block
    let (tx_root, tx_path) = DataTransactionLedger::merklize_tx_root(&sorted_headers);
    assert_eq!(tx_root, new_block.data_ledgers[DataLedger::Submit].tx_root);

    // Create the new epoch to expand the submit ledger to 2 slots
    let epoch_block = genesis_node.mine_block().await?;
    info!(
        "epoch_block:\n{:?}",
        epoch_block.data_ledgers[DataLedger::Submit]
    );

    genesis_node
        .wait_until_height_confirmed(new_block.height, 20)
        .await?;
    genesis_node
        .wait_until_block_index_height(new_block.height, 20)
        .await?;

    let max_submit_chunk_offset = new_block.data_ledgers[DataLedger::Submit]
        .total_chunks
        .saturating_sub(1);
    genesis_node
        .wait_until_block_bounds_available(
            DataLedger::Submit,
            LedgerChunkOffset::from(max_submit_chunk_offset),
            20,
        )
        .await?;

    // Get the epoch snapshot of the latest block
    let block_tree = genesis_node.node_ctx.block_tree_guard.clone();
    let epoch_snapshot = block_tree
        .read()
        .get_epoch_snapshot(&epoch_block.block_hash)
        .expect("to look up the epoch snapshot");

    // Setup some working variables
    let block_index_guard = &genesis_node.node_ctx.block_index_guard.clone();
    let block_tree_guard = &genesis_node.node_ctx.block_tree_guard.clone();
    let num_chunks_in_partition = node_config.consensus_config().num_chunks_in_partition;
    let entropy_packing_iterations = node_config.consensus_config().entropy_packing_iterations;
    let chain_id = node_config.consensus_config().chain_id;

    // Loop though the tx and their chunks validating entropy and data PoAs for each chunk
    for tx_index in 0..3 {
        for chunk_index in 0..3 {
            // Calculate the various chunk offsets
            let ledger_chunk_offset = tx_index as u64 * 3 /*chunks per tx */ + chunk_index as u64;
            let slot_index = (ledger_chunk_offset / num_chunks_in_partition) as usize;
            let partition_chunk_offset =
                (ledger_chunk_offset - (slot_index as u64 * num_chunks_in_partition)) as u32;

            // Lookup the partition assignment and partition_hash for this chunk offset
            let partition_assignments =
                epoch_snapshot.get_partition_assignments(genesis_signer.address());

            let pa = partition_assignments
                .iter()
                .find(|pa| pa.slot_index == Some(slot_index))
                .expect("to find partition assignment for slot");

            let partition_hash = pa.partition_hash;

            // Create the entropy chunk
            let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
            compute_entropy_chunk(
                genesis_signer.address(),
                partition_chunk_offset as u64,
                partition_hash.into(),
                entropy_packing_iterations,
                chunk_size,
                &mut entropy_chunk,
                chain_id,
            );

            // Get the bytes of the unpacked chunk from the transaction
            let mut poa_chunk = if let Some(data) = &sorted_txs[tx_index].data {
                let data = &data.0; // assuming there's a tuple wrapper
                data[chunk_index * chunk_size
                    ..std::cmp::min((chunk_index + 1) * chunk_size, data.len())]
                    .to_vec()
            } else {
                panic!("cannot retrieve chunk from tx");
            };

            // Create the data chunk
            xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

            info!("checking partition chunk offset {partition_chunk_offset} as poa");

            // Create a PoA for the entropy chunk
            let entropy_poa = PoaData {
                tx_path: None,
                data_path: None,
                chunk: Some(Base64(entropy_chunk)),
                ledger_id: Some(DataLedger::Submit.into()),
                partition_chunk_offset,
                partition_hash,
            };

            // Validate both the entropy chunk PoA. Parent for the hypothetical
            // validating block is `new_block` — its height anchors the PoA
            // pre-check on a parent-deterministic view of the chain.
            poa_is_valid(
                &entropy_poa,
                block_index_guard,
                block_tree_guard,
                new_block.block_hash,
                new_block.height,
                &epoch_snapshot,
                &node_config.consensus_config(),
                &genesis_signer.address(),
            )?;

            // Create a PoA for the data chunks
            let data_poa = PoaData {
                tx_path: Some(Base64(tx_path[tx_index].proof.clone())),
                data_path: Some(Base64(
                    sorted_txs[tx_index].proofs[chunk_index].proof.clone(),
                )),
                chunk: Some(Base64(poa_chunk.clone())),
                ledger_id: Some(DataLedger::Submit.into()),
                partition_chunk_offset,
                partition_hash,
            };

            // Validate the data chunk PoA
            poa_is_valid(
                &data_poa,
                block_index_guard,
                block_tree_guard,
                new_block.block_hash,
                new_block.height,
                &epoch_snapshot,
                &node_config.consensus_config(),
                &genesis_signer.address(),
            )?;
        }
    }

    // Orderly shutdown
    genesis_node.stop().await;

    Ok(())
}

//==============================================================================
// Regression: data-PoA at the tip validates via block_tree fallback
//==============================================================================
/// Regression test for the `block_tree` fallback that fixes the P0 #2 stall
/// in REVIEW.md — locks in that data-PoA blocks at the tip validate even
/// before the parent is migrated to the `block_index`.
///
/// Prior to the fix, `poa_is_valid`'s data-path required the parent's item
/// to be in the persistent `block_index`, which only contains blocks
/// migrated `block_migration_depth` deep below the canonical tip. With the
/// mainnet default `block_migration_depth = 6`, a block at height `H` only
/// migrates once the canonical tip reaches `H + 6`, so when a child of `H`
/// arrived for pre-validation the lookup `block_index.get_item(H)` returned
/// `None` and `poa_is_valid` returned `PreValidationError::ParentNotIndexedYet`,
/// stalling the child indefinitely.
///
/// The fix walks the parent chain in `block_tree` when the parent is not
/// yet indexed (the un-migrated window). The config invariant
/// `block_tree_depth > block_migration_depth` guarantees `block_tree`
/// always covers that window.
///
/// Distinct from `multi_slot_poa_test` (above), which forces migration by
/// overriding `block_migration_depth = 1` and explicitly awaiting
/// `wait_until_block_index_height` — masking the at-tip code path.
#[tokio::test]
async fn data_poa_at_tip_validates_via_block_tree_fallback() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "info") };
    initialize_tracing();

    let seconds_to_wait = 20;
    let chunk_size: usize = 32;

    // Use the mainnet default `block_migration_depth = 6`. This is the whole
    // point of the test — with depth=1 (as in `multi_slot_poa_test`) migration
    // catches up after a couple of blocks, and the at-tip lookup never fires
    // `ParentNotIndexedYet`.
    let node_config = NodeConfig::testing().with_consensus(|consensus| {
        consensus.chunk_size = chunk_size as u64;
        consensus.num_partitions_per_slot = 3;
        consensus.epoch.num_blocks_in_epoch = 2;
        consensus.num_chunks_in_partition = 6;
        consensus.num_chunks_in_recall_range = 2;
        consensus.entropy_packing_iterations = 1_000;
        consensus.block_migration_depth = 6;
    });

    let genesis_node = IrysNodeTest::new_genesis(node_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    genesis_node.stop_mining();
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();

    // One tx, three chunks — fits in submit slot 0 (num_chunks_in_partition = 6).
    let tx_chunks = vec![[0_u8; 32], [1_u8; 32], [2_u8; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &tx_chunks {
        data.extend_from_slice(chunk);
    }

    let tx: DataTransaction = genesis_node
        .create_signed_data_tx(&genesis_signer, data)
        .await
        .expect("to create a signed data tx");

    // Post and mine exactly one block — this is `data_block`, the block whose
    // data-PoA we will validate as the "parent" of a hypothetical child block.
    genesis_node.post_data_tx_raw(&tx.header).await;
    let data_block = genesis_node.mine_block().await?;
    info!(
        "data_block: height={} submit_ledger={:?}",
        data_block.height,
        data_block.data_ledgers[DataLedger::Submit]
    );

    // Confirm the submit ledger contains our tx, and grab its tx-root path.
    let sorted_headers = vec![tx.header.clone()];
    let (tx_root, tx_path) = DataTransactionLedger::merklize_tx_root(&sorted_headers);
    assert_eq!(
        tx_root,
        data_block.data_ledgers[DataLedger::Submit].tx_root,
        "submit ledger tx_root must match our single tx"
    );

    // Crucial: do NOT call `wait_until_block_index_height(data_block.height, ...)`
    // here — that would mask the bug by waiting for migration to catch up. With
    // `block_migration_depth = 6` and only one block past genesis (so the tip
    // is height 1), migration needs the tip to reach `1 + 6 = 7` before
    // data_block can be indexed; we deliberately stay well short of that.
    let block_index_guard = genesis_node.node_ctx.block_index_guard.clone();
    assert!(
        block_index_guard
            .read()
            .get_item(data_block.height)
            .is_none(),
        "data_block at height {} must NOT be in the block_index yet \
         (block_migration_depth = 6, tip has not advanced enough to trigger migration); \
         if this assertion fails, the harness migrated faster than expected — \
         re-tune the test rather than weakening the assertion",
        data_block.height
    );

    // Epoch snapshot for data_block (non-epoch block inherits the genesis epoch
    // snapshot since num_blocks_in_epoch = 2 and data_block is at height 1).
    let block_tree = genesis_node.node_ctx.block_tree_guard.clone();
    let epoch_snapshot = block_tree
        .read()
        .get_epoch_snapshot(&data_block.block_hash)
        .expect("to look up the epoch snapshot for data_block");

    // Locate the submit-ledger slot-0 partition assignment for this miner so we
    // can build a PoA against partition_chunk_offset = 0 (the first chunk of
    // the first tx in the submit ledger).
    let partition_assignments = epoch_snapshot.get_partition_assignments(genesis_signer.address());
    let pa = partition_assignments
        .iter()
        .find(|pa| pa.slot_index == Some(0) && pa.ledger_id == Some(DataLedger::Submit.into()))
        .expect("to find submit slot 0 partition assignment");
    let partition_hash = pa.partition_hash;

    // Build the entropy chunk for partition_chunk_offset = 0.
    let num_chunks_in_partition = node_config.consensus_config().num_chunks_in_partition;
    let entropy_packing_iterations = node_config.consensus_config().entropy_packing_iterations;
    let chain_id = node_config.consensus_config().chain_id;
    let partition_chunk_offset: u32 = 0;
    let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
    compute_entropy_chunk(
        genesis_signer.address(),
        partition_chunk_offset as u64,
        partition_hash.into(),
        entropy_packing_iterations,
        chunk_size,
        &mut entropy_chunk,
        chain_id,
    );
    let _ = num_chunks_in_partition; // kept for symmetry with multi_slot_poa_test

    // Pack the first chunk of the first tx (XOR with entropy).
    let mut poa_chunk = tx
        .data
        .as_ref()
        .map(|d| d.0[..chunk_size].to_vec())
        .expect("tx must carry data");
    xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

    // Construct a data-PoA — all of {tx_path, data_path, ledger_id} must be
    // Some(_) to trigger the data-path branch in `poa_is_valid`.
    let data_poa = PoaData {
        tx_path: Some(Base64(tx_path[0].proof.clone())),
        data_path: Some(Base64(tx.proofs[0].proof.clone())),
        chunk: Some(Base64(poa_chunk)),
        ledger_id: Some(DataLedger::Submit.into()),
        partition_chunk_offset,
        partition_hash,
    };

    // Re-check immediately before the call — this nails down "parent not in
    // index" at the moment of evaluation, ruling out a race where migration
    // ran between the prior assertion and the `poa_is_valid` call.
    assert!(
        block_index_guard
            .read()
            .get_item(data_block.height)
            .is_none(),
        "data_block must still be un-migrated at the moment of poa_is_valid"
    );

    // Call `poa_is_valid` with `parent_height = data_block.height` — i.e. as
    // if we were pre-validating a hypothetical child block built on top of
    // `data_block`. With the fix in place, `poa_is_valid` falls back to
    // `block_tree` (because `block_index.get_item(data_block.height)` is
    // None — asserted twice above) and resolves the bounds from there.
    let result = poa_is_valid(
        &data_poa,
        &block_index_guard,
        &block_tree,
        data_block.block_hash,
        data_block.height,
        &epoch_snapshot,
        &node_config.consensus_config(),
        &genesis_signer.address(),
    );

    result.expect(
        "data-PoA at the tip should validate via block_tree fallback \
         (parent un-migrated; bounds resolved from block_tree)",
    );
    info!(
        "validated data-PoA at tip via block_tree fallback at parent_height={}",
        data_block.height
    );

    // Orderly shutdown
    genesis_node.stop().await;

    Ok(())
}
