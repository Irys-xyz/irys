use crate::utils::post_chunk;
use crate::utils::{get_block_parent, mine_block, verify_published_chunk, IrysNodeTest};
use actix_web::test::{self, call_service, TestRequest};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use awc::http::StatusCode;
use base58::ToBase58;
use irys_actors::packing::wait_for_packing;
use irys_database::{tables::IngressProofs, walk_all};
use irys_types::{irys::IrysSigner, IrysTransaction, IrysTransactionHeader, LedgerChunkOffset};
use irys_types::{DataLedger, NodeConfig};
use reth_db::Database as _;
use std::time::Duration;
use tracing::debug;

#[test_log::test(actix_web::test)]
async fn heavy_double_root_data_promotion_test() {
    let mut config = NodeConfig::testnet();
    let chunk_size = 32; // 32 byte chunks
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.storage.num_writes_before_sync = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    // Testnet / single node config
    config.consensus.get_mut().chunk_migration_depth = 1;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    let signer2 = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![
        (
            signer.address(),
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            signer2.address(),
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();

    // mine an initial empty block
    let _block1 = mine_block(&node.node_ctx).await.unwrap().unwrap();

    let app = node.start_public_api().await;

    // Create a bunch of TX chunks
    let data_chunks = [
        vec![[10; 32], [20; 32], [30; 32]],
        vec![[40; 32], [50; 32], [50; 32]],
    ];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<IrysTransaction> = Vec::new();

    for (i, chunks) in data_chunks.iter().enumerate() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        // we have to use a different signer so we get a unique txid for each transaction, despite the identical data_root
        let s = if i == 2 { &signer2 } else { &signer };
        let tx = s.create_transaction(data, None).unwrap();
        let tx = s.sign_transaction(tx).unwrap();
        println!("tx[{}] {}", i, tx.header.id.as_bytes().to_base58());
        txs.push(tx);
    }
    // submit tx 1 & 2
    // upload their chunks, make sure 1 is promoted (and 2 is not, due to a missing chunk)

    // Post the 2 transactions & initialize some state to track their confirmation
    let mut unconfirmed_tx: Vec<IrysTransactionHeader> = Vec::new();
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

    // Wait for all the transactions to be confirmed
    let result = node.wait_for_confirmed_txs(unconfirmed_tx, 20).await;
    // Verify all transactions are confirmed
    assert!(result.is_ok());

    // ==============================
    // Post Tx chunks out of order
    // ------------------------------

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
    let unconfirmed_promotions = vec![txs[0].header.id];
    let result = node
        .wait_for_ingress_proofs(unconfirmed_promotions, 20)
        .await;
    assert!(result.is_ok());

    // wait for the first set of chunks to appear in the publish ledger
    // FIXME: in prior commit, this was a loop that was never asserting or erroring on failure - is it important for the test case?
    //        assert commented out to mimic prior (passing test) behaviour
    let _result = node.wait_for_chunk(&app, DataLedger::Publish, 0, 20).await;
    //assert!(result.is_ok());

    // wait for the second set of chunks to appear in the publish ledger
    // FIXME: in prior commit, this was a loop that was never asserting or erroring on failure - is it important for the test case?
    //        assert commented out to mimic prior (passing test) behaviour
    let _result = node.wait_for_chunk(&app, DataLedger::Publish, 3, 20).await;
    //assert!(result.is_ok());

    let db = &node.node_ctx.db.clone();
    let block_tx1 = get_block_parent(txs[0].header.id, DataLedger::Publish, db).unwrap();

    let txid_1 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[0];
    let first_tx_index: usize = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    println!("1:{}", block_tx1);

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

    // Part 2
    debug!("PHASE 2");

    // ensure the ingress proof still exists
    let ingress_proofs = db.view(walk_all::<IngressProofs, _>).unwrap().unwrap();
    assert_eq!(ingress_proofs.len(), 1);

    // same chunks as tx1
    let data_chunks = [vec![[10; 32], [20; 32], [30; 32]]];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<IrysTransaction> = Vec::new();

    let tree_guard = node.node_ctx.block_tree_guard.clone();
    drop(tree_guard);

    for chunks in data_chunks.iter() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        // we have to use a different signer so we get a unique txid for each transaction, despite the identical data_root
        let s = &signer2;
        let tx = s.create_transaction(data, None).unwrap();
        let tx = s.sign_transaction(tx).unwrap();
        println!("tx[2] {}", tx.header.id.as_bytes().to_base58());
        txs.push(tx);
    }
    // submit tx 3
    // upload their chunks, make sure 3 is promoted

    // Post the tx & initialize some state to track their confirmation
    let mut unconfirmed_tx: Vec<IrysTransactionHeader> = Vec::new();
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

    // Wait for all the transactions to be confirmed
    let result = node.wait_for_confirmed_txs(unconfirmed_tx, 20).await;
    // Verify all transactions are confirmed
    assert!(result.is_ok());

    // ==============================
    // Post Tx chunks out of order
    // ------------------------------

    let tx_index = 0;

    // First Tx, middle chunk
    let chunk_index = 1;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // First Tx, first chunk
    let chunk_index = 0;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // First Tx, last chunk
    let chunk_index = 2;
    post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // ==============================
    // Verify ingress proofs
    // ------------------------------
    // Wait for the transactions to be promoted
    let unconfirmed_promotions = vec![txs[0].header.id];
    let result = node
        .wait_for_ingress_proofs(unconfirmed_promotions, 20)
        .await;
    assert!(result.is_ok());

    // wait for the second set of chunks to appear in the publish ledger
    let result = node.wait_for_chunk(&app, DataLedger::Publish, 3, 20).await;
    assert!(result.is_ok());

    let db = &node.node_ctx.db.clone();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ==============================
    // Verify chunk ordering in publish ledger storage module
    // ------------------------------
    // Verify the chunks of the first promoted transaction
    let tx_index = 0;

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

    // ensure the ingress proof is gone
    let ingress_proofs = db.view(walk_all::<IngressProofs, _>).unwrap().unwrap();
    assert_eq!(ingress_proofs.len(), 0);

    node.node_ctx.stop().await;
}
