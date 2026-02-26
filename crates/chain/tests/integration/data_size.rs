use irys_chain::IrysNodeCtx;
use irys_domain::ChunkType;
use irys_testing_utils::initialize_tracing;
use irys_types::{DataLedger, LedgerChunkOffset, NodeConfig};
use tracing::info;

use crate::utils::IrysNodeTest;

#[tokio::test]
async fn test_overlapping_data_sizes() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };
    initialize_tracing();

    // Create a node
    let seconds_to_wait = 20;
    let chunk_size: usize = 32;

    // 1. Configure network
    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = chunk_size as u64;
            consensus.num_partitions_per_slot = 1;
            consensus.num_chunks_in_partition = 10;
            consensus.epoch.num_blocks_in_epoch = 4;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
            consensus.block_migration_depth = 1;
            consensus.epoch.submit_ledger_epoch_length = 1000;
        })
        .with_genesis_peer_discovery_timeout(1000);

    // Start the node
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();

    // But keep it from mining
    genesis_node.stop_mining();

    // Create a set of chunks for tx data
    let chunks = [[10; 32], [20; 32], [30; 32], [40; 32], [50; 32], [60; 32]];
    let data: Vec<u8> = chunks.concat();

    // Create a second set of chunks for a different data_root tx
    let chunks2 = [[11; 32], [21; 32], [31; 32], [41; 32], [51; 32], [61; 32]];
    let data2: Vec<u8> = chunks2.concat();

    // Compose a valid transaction with all of the chunks and accurate data_size
    let valid_tx = genesis_node
        .create_signed_data_tx(&genesis_signer, data.clone())
        .await?;

    // Use the data_root of that transaction to compose another with a single chunk data_size
    let data_root = valid_tx.header.data_root;
    let bad_data_size = 32 * 3; // 3 chunks worth

    // Query the price endpoint to get required fees
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, bad_data_size)
        .await
        .expect("Failed to get price");

    let mut wrong_data_size_tx = genesis_signer.create_publish_transaction(
        data,
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;

    wrong_data_size_tx.header.data_root = data_root;
    wrong_data_size_tx.header.data_size = bad_data_size;
    wrong_data_size_tx = genesis_signer.sign_transaction(wrong_data_size_tx)?;

    // Post the too small data_size tx first
    genesis_node
        .post_data_tx_raw(&wrong_data_size_tx.header)
        .await;

    // Wait for it to be migrated so it appears in the ledger first
    let wrong_data_size_header = wrong_data_size_tx.header.clone();
    genesis_node
        .wait_for_migrated_txs(vec![wrong_data_size_header], seconds_to_wait)
        .await?;

    // Post the last 3 chunks - they should be parked (not rejected) since the
    // cached data_size is unconfirmed.
    for i in 3..6 {
        let (status, _body) = genesis_node
            .post_chunk_32b_with_status(&valid_tx, i, &chunks)
            .await;
        info!("{:#?}", status);
        // Chunks are parked when cached data_size is unconfirmed and chunk claims larger size
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    // Post the valid tx to adjust the data_size for the data_root
    genesis_node.post_data_tx_raw(&valid_tx.header).await;

    genesis_node
        .wait_for_mempool(valid_tx.header.id, seconds_to_wait)
        .await?;

    // The parked chunks should now be processed since the cache was upgraded.
    // Re-posting should still succeed.
    for i in 3..6 {
        let (status, _body) = genesis_node
            .post_chunk_32b_with_status(&valid_tx, i, &chunks)
            .await;
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    // Mine a block (to migrate the wrong_data_size_tx)
    genesis_node.mine_block().await?;

    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, bad_data_size)
        .await
        .expect("Failed to get price");

    // Also post the second bad tx (splitting these up over blocks enforces their order in the ledger)
    let mut wrong_data_size_tx2 = genesis_signer.create_publish_transaction(
        data2.clone(),
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    wrong_data_size_tx2.header.data_size = bad_data_size;
    wrong_data_size_tx2 = genesis_signer.sign_transaction(wrong_data_size_tx2)?;

    genesis_node
        .post_data_tx_raw(&wrong_data_size_tx2.header)
        .await;

    genesis_node.mine_block().await?;

    // Validate the chunks do not appear in the ledger
    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 0);

    // Post the first 3 chunks and verify they end up in the right places
    for i in 0..3 {
        let (status, _body) = genesis_node
            .post_chunk_32b_with_status(&valid_tx, i, &chunks)
            .await;
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    genesis_node.mine_blocks(3).await?;

    // Verify the chunks of the first and second tx
    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 0);
    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Publish, 0);

    // Get the first chunk from publish ledger to determine promotion order
    let first_chunk = genesis_node
        .get_chunk(DataLedger::Publish, LedgerChunkOffset::from(0))
        .await
        .expect("the publish ledger chunk should exist");

    // For the publish ledger, track the offsets of the valid and invalid tx
    // as the submit tx may have been promoted in any order
    let (valid_publish_offset, wrong_publish_offset) =
        if first_chunk.data_size == valid_tx.header.data_size {
            (0, 6) // valid_tx promoted first
        } else {
            (3, 0) // wrong_data_size_tx promoted first
        };

    // Validate the 3 wrong_data_size_tx chunks in both ledgers
    for i in 0..3 {
        // Verify presence in Submit
        genesis_node
            .verify_migrated_chunk_32b(
                DataLedger::Submit,
                LedgerChunkOffset::from(i as u64),
                &chunks[i],
                wrong_data_size_tx.header.data_size,
            )
            .await;

        // Verify absence in Publish at wrong offset
        genesis_node
            .verify_chunk_not_present(
                DataLedger::Publish,
                LedgerChunkOffset::from((i + wrong_publish_offset) as u64),
            )
            .await;
    }

    // Validate the 6 valid_tx chunks in both ledgers
    for i in 0..6 {
        for (ledger, offset) in [
            (DataLedger::Submit, 3),
            (DataLedger::Publish, valid_publish_offset),
        ] {
            genesis_node
                .verify_migrated_chunk_32b(
                    ledger,
                    LedgerChunkOffset::from((i + offset) as u64),
                    &chunks[i],
                    valid_tx.header.data_size,
                )
                .await;
        }
    }

    // Post the first 3 chunk of the second wrong size tx
    for i in 0..3 {
        let (status, _body) = genesis_node
            .post_chunk_32b_with_status(&wrong_data_size_tx2, i, &chunks2)
            .await;
        assert_eq!(status, reqwest::StatusCode::OK);
    }

    genesis_node.mine_blocks(3).await?;

    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 0);
    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 1);
    check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Publish, 0);

    // Validate the chunks of wrong_data_size_tx2 (the final tx in the ledger)
    for i in 0..3 {
        genesis_node
            .verify_migrated_chunk_32b(
                DataLedger::Submit,
                LedgerChunkOffset::from(9 + i as u64),
                &chunks2[i],
                bad_data_size,
            )
            .await;
    }

    // Validate that the wrong_data_size_tx2 do not promote
    let invalid_publish_chunk = genesis_node
        .get_chunk(DataLedger::Publish, LedgerChunkOffset::from(9))
        .await;
    assert!(invalid_publish_chunk.is_none());

    // Graceful shutdown
    genesis_node.stop().await;

    // Check the publish ledger
    Ok(())
}

fn check_storage_module_chunks(
    node: &IrysNodeTest<IrysNodeCtx>,
    name: &str,
    ledger: DataLedger,
    slot_index: usize,
) {
    let data_intervals = node.get_storage_module_intervals(ledger, slot_index, ChunkType::Data);
    let packed_intervals =
        node.get_storage_module_intervals(ledger, slot_index, ChunkType::Entropy);

    // Extract the offsets
    let mut data_chunks = Vec::new();
    for int in data_intervals {
        let start: u32 = int.start().into();
        let end: u32 = int.end().into();
        for offset in start..=end {
            data_chunks.push(offset);
        }
    }

    let mut packed_chunks = Vec::new();
    for int in packed_intervals {
        let start: u32 = int.start().into();
        let end: u32 = int.end().into();
        for offset in start..=end {
            packed_chunks.push(offset);
        }
    }

    info!(
        "\n{}: {:?}:{}\n data offsets: {:?}\n pack offsets: {:?}\n",
        name, ledger, slot_index, data_chunks, packed_chunks
    );
}
