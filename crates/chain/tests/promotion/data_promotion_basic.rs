use crate::utils::{get_block_parent, post_chunk, verify_published_chunk};
use crate::utils::{AddTxError, IrysNodeTest};
use actix_web::http::StatusCode;
use actix_web::test::{self, call_service, TestRequest};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use assert_matches::assert_matches;
use irys_actors::MempoolServiceMessage;
use irys_database::db::IrysDatabaseExt as _;
use irys_testing_utils::initialize_tracing;
use irys_types::ingress::generate_ingress_proof;
use irys_types::SendTraced as _;
use irys_types::{irys::IrysSigner, DataTransaction, DataTransactionHeader, LedgerChunkOffset};
use irys_types::{DataLedger, NodeConfig};
use std::time::Duration;
use tracing::{debug, info, warn};

#[test_log::test(tokio::test)]
async fn heavy_data_promotion_test() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.storage.num_writes_before_sync = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().block_migration_depth = 1; // Testnet / single node config
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    let app = node.start_public_api().await;

    // Create a bunch of TX chunks
    let data_chunks = [
        vec![[10; 32], [20; 32], [30; 32]],
        vec![[40; 32], [50; 32], [50; 32]],
        vec![[70; 32], [80; 32], [90; 32]],
    ];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<DataTransaction> = Vec::new();

    for (i, chunks) in data_chunks.iter().enumerate() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }

        // Get price from the API
        let price_info = node
            .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
            .await
            .expect("Failed to get price");

        let tx = signer
            .create_publish_transaction(
                data,
                node.get_anchor().await?,
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        println!("tx[{}] {}", i, tx.header.id);
        txs.push(tx);
    }

    // Post the 3 transactions & initialize some state to track their confirmation
    let mut unconfirmed_tx: Vec<DataTransactionHeader> = Vec::new();
    for tx in txs.iter() {
        let header = &tx.header;
        unconfirmed_tx.push(header.clone());
        let req = TestRequest::post()
            .uri("/v1/tx")
            .set_json(header)
            .to_request();

        let resp = call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        debug!("Response body: {:#?}", body);
        assert_eq!(status, StatusCode::OK);
    }

    // Wait for all the transactions to be in the index
    let result = node.wait_for_migrated_txs(unconfirmed_tx, 10).await;
    // Verify all transactions are in the index
    assert!(result.is_ok());

    // ==============================
    // Post Tx chunks out of order
    // ------------------------------
    let tx_index = 2;

    // Last Tx, last chunk
    let chunk_index = 2;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // Last Tx, middle chunk
    let chunk_index = 1;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // Last Tx, first chunk
    let chunk_index = 0;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    let tx_index = 1;

    // Middle Tx, middle chunk
    let chunk_index = 1;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // Middle Tx, first chunk
    let chunk_index = 0;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    //-----------------------------------------------
    // Note: Middle Tx, last chunk is never posted
    //-----------------------------------------------

    let tx_index = 0;

    // First Tx, first chunk
    let chunk_index = 0;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // First Tx, middle chunk
    let chunk_index = 1;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // First Tx, last chunk
    let chunk_index = 2;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // ==============================
    // Verify ingress proofs
    // ------------------------------
    // Wait for the transactions to be promoted
    let unconfirmed_promotions = vec![txs[2].header.id, txs[0].header.id];
    let result = node
        .wait_for_ingress_proofs(unconfirmed_promotions, 20)
        .await;
    assert!(result.is_ok());

    // mine a block
    node.mine_block().await?;

    // wait for the first set of chunks to appear in the publish ledger
    let result = node.wait_for_chunk(&app, DataLedger::Publish, 0, 20).await;
    assert!(result.is_ok());
    // wait for the second set of chunks to appear in the publish ledger
    let result = node.wait_for_chunk(&app, DataLedger::Publish, 3, 20).await;
    assert!(result.is_ok());

    let db = &node.node_ctx.db.clone();
    let block_tx1 = get_block_parent(txs[0].header.id, DataLedger::Publish, db).unwrap();
    let block_tx2 = get_block_parent(txs[2].header.id, DataLedger::Publish, db).unwrap();

    let first_tx_index: usize;
    let next_tx_index: usize;

    if block_tx1.block_hash == block_tx2.block_hash {
        // Extract the transaction order
        let txid_1 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[0];
        let txid_2 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[1];
        first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
        next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
        println!("1:{:?}", block_tx1);
    } else if block_tx1.height > block_tx2.height {
        let txid_1 = block_tx2.data_ledgers[DataLedger::Publish].tx_ids.0[0];
        let txid_2 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[0];
        first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
        next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
        println!("1:{:?}", block_tx2);
        println!("2:{:?}", block_tx1);
    } else {
        let txid_1 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[0];
        let txid_2 = block_tx2.data_ledgers[DataLedger::Publish].tx_ids.0[0];
        first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
        next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
        println!("1:{:?}", block_tx1);
        println!("2:{:?}", block_tx2);
    }

    // ==============================
    // Verify chunk ordering in publish ledger storage module
    // ------------------------------
    // Verify the chunks of the first promoted transaction
    let tx_index = first_tx_index;

    let chunk_offset = 0;
    let expected_bytes = &data_chunks[tx_index][0];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    let chunk_offset = 1;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    let chunk_offset = 2;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    // Verify the chunks of the second promoted transaction
    let tx_index = next_tx_index;

    let chunk_offset = 3;
    let expected_bytes = &data_chunks[tx_index][0];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    let chunk_offset = 4;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    let chunk_offset = 5;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &node.node_ctx.config,
    )
    .await;

    node.stop().await;

    Ok(())
}

// This test simulates a case encountered on testnet, where a submit tx was not able to be included in a block, but it was a promotion candidate.
#[actix_web::test]
async fn heavy_promotion_validates_submit_inclusion_test() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    initialize_tracing();

    let seconds_to_wait = 30;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.epoch.num_blocks_in_epoch = 3;
            consensus.block_migration_depth = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // mine some blocks
    genesis_node.mine_blocks(5).await?;
    let blk5 = genesis_node.get_block_by_height(5).await?;

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    // we create a transaction with an anchor that is too new for inclusion in a block
    let data_tx = {
        // Get data size before moving data
        let data_size = data.len() as u64;

        // Query the price endpoint to get required fees for Publish ledger
        let price_info = genesis_node
            .get_data_price(DataLedger::Publish, data_size)
            .await
            .map_err(AddTxError::CreateTx)?;

        // Create transaction with proper fees
        let tx = genesis_signer
            .create_publish_transaction(
                data,
                blk5.block_hash, // anchor
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .map_err(AddTxError::CreateTx)?;

        genesis_signer
            .sign_transaction(tx)
            .map_err(AddTxError::CreateTx)
    }?;

    // we then submit it as gossip so it bypasses the at-ingress check for anchor maturity
    let (tx, rx) = tokio::sync::oneshot::channel();
    // ingest as gossip
    genesis_node
        .node_ctx
        .service_senders
        .mempool
        .send_traced(MempoolServiceMessage::IngestDataTxFromGossip(
            data_tx.header.clone(),
            tx,
        ))
        .map_err(|_| eyre::eyre!("failed to send mempool message"))?;
    // Ignore possible ingestion errors in tests
    let _ = rx.await?;

    // the tx should now be in the mempool - it should not be included in the submit ledger, but it should be a publish candidate
    genesis_node
        .wait_for_mempool(data_tx.header.id, seconds_to_wait)
        .await?;

    genesis_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 2, &chunks).await;

    let res = genesis_node
        .wait_for_ingress_proofs_no_mining(vec![data_tx.header.id], seconds_to_wait)
        .await;

    assert_matches!(res, Ok(()));

    // assert that the tx is not promoted and wasn't included in any blocks
    let block = genesis_node.mine_block().await?;
    let is_promoted = genesis_node.get_is_promoted(&data_tx.header.id).await?;
    assert!(!is_promoted);

    assert_eq!(
        block.data_ledgers.iter().fold(0, |n, l| n + l.tx_ids.len()),
        0
    );

    // now we wait for the tx to have an old enough anchor
    genesis_node
        .mine_blocks(config.consensus_config().block_migration_depth as usize + 1)
        .await?;
    // ..and now it should be promoted!
    let is_promoted = genesis_node.get_is_promoted(&data_tx.header.id).await?;
    assert!(is_promoted);

    // Wind down test
    genesis_node.stop().await;

    Ok(())
}

#[actix_web::test]
async fn heavy_promotion_validates_ingress_proof_anchor() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    initialize_tracing();

    let seconds_to_wait = 30;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.epoch.num_blocks_in_epoch = 3;
            consensus.block_migration_depth = 1;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.mempool.ingress_proof_anchor_expiry_depth = 5;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // mine some blocks
    genesis_node.mine_blocks(5).await?;
    // let blk5 = genesis_node.get_block_by_height(5).await?;

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let data_tx = genesis_signer.create_publish_transaction(
        data.clone(),
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let data_tx = genesis_signer.sign_transaction(data_tx)?;

    genesis_node.post_data_tx_raw(&data_tx.header).await;

    genesis_node
        .wait_for_mempool(data_tx.header.id, seconds_to_wait)
        .await?;

    // now we generate an ingress proof anchored to the genesis block
    // this should be too old, and should be rejected by the API.
    let too_old_ingress_proof = generate_ingress_proof(
        &genesis_signer,
        data_tx.header.data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        genesis_node.get_block_by_height(0).await?.block_hash,
    )?;

    // ensure we are past the 5-block anchor depth for ingress proofs
    genesis_node
        .mine_blocks(6 - genesis_node.get_canonical_chain_height().await as usize)
        .await?;

    // submit
    let resp = genesis_node
        .ingest_ingress_proof(too_old_ingress_proof.clone())
        .await;

    // TODO proper error typing
    assert!(resp.is_err_and(|e| e.to_string().starts_with("Invalid anchor")));

    // now we submit an ingress proof with a newer anchor:
    let new_ingress_proof = generate_ingress_proof(
        &genesis_signer,
        data_tx.header.data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        genesis_node.get_anchor().await?,
    )?;

    // submit - should be accepted
    let resp = genesis_node
        .ingest_ingress_proof(new_ingress_proof.clone())
        .await;

    // TODO proper error typing
    assert!(resp.is_ok());

    // Wait for ingress proof to be stored in DB before mining.
    // BlockConfirmed handler looks up proofs from DB to determine promotability.
    // If we mine before the proof is persisted, promotion won't happen.
    genesis_node
        .wait_for_ingress_proofs_no_mining(vec![data_tx.header.id], seconds_to_wait)
        .await?;

    // Mine a block - the ingress proof should enable promotion
    genesis_node.mine_block().await?;

    // Poll for promotion status - mempool processes BlockConfirmed asynchronously
    // Use 100ms intervals for faster detection (300 checks over 30 seconds)
    let mut is_promoted = false;
    for _ in 0..(seconds_to_wait * 10) {
        if genesis_node.get_is_promoted(&data_tx.header.id).await? {
            is_promoted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(is_promoted, "Transaction was not promoted within timeout");

    // Wind down test
    genesis_node.stop().await;

    Ok(())
}

/// This test is a regression test that ensures that ingress proofs with edge-case invalid anchors (miss the expiry by one block height, but are valid at ingest) are not accepted by the node as part of validation, and are also not selected as part of the block building process.
#[tokio::test]
async fn heavy_promotion_validates_ingress_proof_anchor_edge_doesnt_promote() -> eyre::Result<()> {
    test_ingress_proof_anchor_edge_case(0, false).await
}

/// This test is a regression test that ensures that ingress proofs with edge-case valid anchors (exactly at the minimum expiry height, valid at ingest) are accepted by the node as part of validation, and are also selected as part of the block building process.
#[tokio::test]
async fn heavy_promotion_validates_ingress_proof_anchor_edge_does_promote() -> eyre::Result<()> {
    test_ingress_proof_anchor_edge_case(1, true).await
}

/// Helper function to test ingress proof anchor validation edge cases.
/// Tests regression for a bug where ingress proofs with boundary anchor expiry heights
/// could have inconsistent behavior between mempool and block validation.
async fn test_ingress_proof_anchor_edge_case(
    anchor_height_offset: u64,
    should_promote: bool,
) -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    initialize_tracing();

    let seconds_to_wait = 30;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.epoch.num_blocks_in_epoch = 3;
            consensus.block_migration_depth = 1;
            consensus.mempool.tx_anchor_expiry_depth = 3;
            consensus.mempool.ingress_proof_anchor_expiry_depth = 5;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    let genesis_signer = config.signer();

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // This is a secondary regression test for the bug where an anchor at height (current_height - tx_anchor_expiry_depth - 1)
    // passes mempool validation but fails block validation.

    // With tx_anchor_expiry_depth=3 and ingress_proof_anchor_expiry_depth=5:
    // - When mining block at height H:
    //   - min_tx_anchor_height = H - 3
    //   - min_ingress_proof_anchor_height = H - 5
    //   - valid anchors from block tree: H-1, H-2, H-3
    //   - valid anchors from block index: H-5
    //   - MISSING: H-4
    // - An ingress proof at H-4 passes mempool (H-4 >= H-5) but would fail block validation with this bug.

    let ingress_anchor_expiry = config
        .consensus_config()
        .mempool
        .ingress_proof_anchor_expiry_depth as u64;

    genesis_node
        .mine_until_condition(
            |b| b.last().unwrap().height >= ingress_anchor_expiry + 2, // buffer
            1,
            10,
            30,
        )
        .await?;

    let current_height = genesis_node.get_canonical_chain_height().await;

    // calculate the edge case anchor height with the provided offset
    let edge_case_anchor_height = (current_height - ingress_anchor_expiry) + anchor_height_offset;
    let edge_case_block = genesis_node
        .get_block_by_height(edge_case_anchor_height)
        .await?;

    info!(
        "current_height: {}, edge_case_anchor_height:{} (offset: {})",
        current_height, edge_case_anchor_height, anchor_height_offset
    );

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let edge_data_tx = genesis_signer.create_publish_transaction(
        data.clone(),
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let edge_data_tx = genesis_signer.sign_transaction(edge_data_tx)?;

    info!("Edge case transaction ID: {}", edge_data_tx.header.id);

    genesis_node.post_data_tx_raw(&edge_data_tx.header).await;
    genesis_node
        .wait_for_mempool(edge_data_tx.header.id, seconds_to_wait)
        .await?;

    // create an ingress proof anchored at the edge case height
    let edge_case_ingress_proof = generate_ingress_proof(
        &genesis_signer,
        edge_data_tx.header.data_root,
        chunks.iter().copied().map(Ok),
        config.consensus_config().chain_id,
        edge_case_block.block_hash,
    )?;

    info!(
        "Submitting ingress proof with anchor at height {} (block hash: {}, id: {})",
        edge_case_anchor_height,
        edge_case_block.block_hash,
        edge_case_ingress_proof.id()
    );

    // Post chunks first so the node auto-generates an ingress proof once all chunks are cached.
    genesis_node.post_chunk_32b(&edge_data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&edge_data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&edge_data_tx, 2, &chunks).await;

    // Wait for the auto-generated proof (1 proof from genesis_signer).
    // We must wait for this async task to complete before injecting the edge-case proof,
    // otherwise it could race and overwrite the edge-case proof after we insert it below.
    genesis_node
        .wait_for_multiple_ingress_proofs_no_mining(
            vec![edge_data_tx.header.id],
            1,
            seconds_to_wait,
        )
        .await?;

    // Now inject the edge-case ingress proof to verify mempool validation accepts it.
    // This replaces the auto-generated proof (same signer address, dupsort dedup).
    let resp = genesis_node
        .ingest_ingress_proof(edge_case_ingress_proof.clone())
        .await;

    assert!(
        resp.is_ok(),
        "Edge case ingress proof should pass mempool validation but got: {:?}",
        resp.err()
    );

    genesis_node.node_ctx.db.update_eyre(|tx| {
        use reth_db::transaction::DbTxMut as _;
        tx.clear::<irys_database::tables::IngressProofs>()?; // wipe all existing ingress proofs
                                                             // insert just our one, so it always gets included
        irys_database::store_ingress_proof_checked(tx, &edge_case_ingress_proof, &genesis_signer)
    })?;

    // Now mine a block. If the bug exists, this will fail during block discovery
    // because the edge_case_anchor_height is not in valid_ingress_anchor_blocks

    info!("Mining block at height {}...", current_height + 1);
    let mine_result = genesis_node.mine_block().await;

    match mine_result {
        Err(e) => {
            let error_str = format!("{:?}", e);
            if error_str.contains("InvalidAnchor")
                && error_str.contains(&edge_case_block.block_hash.to_string())
            {
                eyre::bail!("Block validation failed with InvalidAnchor error for edge case anchor at height {}", edge_case_anchor_height)
            } else {
                // different error - propagate it
                eyre::bail!("Unexpected error, expected InvalidAnchor, got {}", &e)
            }
        }
        Ok(block) => {
            // check if the edge case transaction was actually promoted in this block
            let publish_tx_ids: Vec<_> = block.data_ledgers[DataLedger::Publish].tx_ids.0.clone();

            if publish_tx_ids.contains(&edge_data_tx.header.id) {
                if should_promote {
                    info!("tx was promoted as expected");
                } else {
                    warn!("Tx was promoted but should NOT have been");
                    eyre::bail!(
                        "Expected submit tx {} to NOT be promoted by ingress proof {}",
                        &edge_data_tx.header.id,
                        &edge_case_ingress_proof.id(),
                    );
                }
            } else if should_promote {
                warn!("Tx was NOT promoted but should have been");
                eyre::bail!(
                    "Expected submit tx {} to be promoted by ingress proof {}",
                    &edge_data_tx.header.id,
                    &edge_case_ingress_proof.id(),
                );
            } else {
                info!("tx was NOT promoted as expected");
            }
        }
    }

    // Wind down test
    genesis_node.stop().await;

    Ok(())
}
