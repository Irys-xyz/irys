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

            // Validate both the entropy chunk PoA
            poa_is_valid(
                &entropy_poa,
                block_index_guard,
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
