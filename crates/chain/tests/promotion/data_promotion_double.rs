use crate::utils::post_chunk;
use crate::utils::{verify_published_chunk, IrysNodeTest};
use actix_web::http::StatusCode;
use actix_web::test::{self, call_service, TestRequest};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;

use irys_database::{tables::IngressProofs, walk_all};
use irys_types::{irys::IrysSigner, DataTransaction, DataTransactionHeader, LedgerChunkOffset};
use irys_types::{DataLedger, NodeConfig};
use reth_db::Database as _;
use std::time::Duration;
use tracing::debug;

#[test_log::test(tokio::test)]
async fn slow_heavy_double_root_data_promotion_test() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    let chunk_size = 32; // 32 byte chunks
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.storage.num_writes_before_sync = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    // Testnet / single node config
    config.consensus.get_mut().block_migration_depth = 1;
    let anchor_expiry_depth = 10;
    config.consensus.get_mut().mempool.tx_anchor_expiry_depth = anchor_expiry_depth;
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

    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    let block1 = node.mine_block().await.expect("expected mined block");

    let app = node.start_public_api().await;

    // Create a bunch of TX chunks
    let data_chunks = [
        vec![[10; 32], [20; 32], [30; 32]],
        vec![[40; 32], [50; 32], [50; 32]],
    ];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<DataTransaction> = Vec::new();

    for (i, chunks) in data_chunks.iter().enumerate() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        // we have to use a different signer so we get a unique txid for each transaction, despite the identical data_root
        let s = if i == 2 { &signer2 } else { &signer };

        // Get price from the API
        let price_info = node
            .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
            .await
            .expect("Failed to get price");

        let tx = s
            .create_publish_transaction(
                data,
                node.get_anchor().await?,
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .unwrap();
        let tx = s.sign_transaction(tx).unwrap();
        println!("tx[{}] {}", i, tx.header.id);
        txs.push(tx);
    }
    // submit tx 1 & 2
    // upload their chunks, make sure 1 is promoted (and 2 is not, due to a missing chunk)

    // Post the 2 transactions & initialize some state to track their confirmation
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

    // Wait for all the transactions to be confirmed
    // this mines blocks
    let result = node.wait_for_migrated_txs(unconfirmed_tx, 20).await;
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
    // this mines blocks
    let result = node
        .wait_for_ingress_proofs(unconfirmed_promotions, 20)
        .await;
    assert!(result.is_ok());
    node.mine_block().await?;

    // Promotion and storage migration are asynchronous; CI can need extra blocks
    // before publish chunks and parent headers become visible.
    node.future_or_mine_on_timeout(
        node.wait_for_chunk(&app, DataLedger::Publish, 0, 60),
        Duration::from_secs(5),
    )
    .await??;

    let db = node.node_ctx.db.clone();
    let block_tx1 = node
        .future_or_mine_on_timeout(
            node.wait_for_block_containing_tx(txs[0].header.id, DataLedger::Publish, 60),
            Duration::from_secs(5),
        )
        .await??;

    let first_tx_id_in_block = block_tx1.data_ledgers[DataLedger::Publish]
        .tx_ids
        .0
        .iter()
        .copied()
        .find(|tid| txs.iter().any(|tx| tx.header.id == *tid))
        .expect("Expected at least one of the test txs to be in the block");
    let first_tx_index: usize = txs
        .iter()
        .position(|tx| tx.header.id == first_tx_id_in_block)
        .expect("Matching tx not found in txs");
    println!("1:{:?}", block_tx1);

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

    // mine 1 block
    let blk = node.mine_block().await.expect("expected mined block");
    debug!("P2 block {}", &blk.height);

    // ensure the ingress proof still exists
    let ingress_proofs = db
        .view(|read_tx| walk_all::<IngressProofs, _>(read_tx))
        .unwrap()
        .unwrap();
    assert_eq!(ingress_proofs.len(), 1);

    // same chunks as tx1
    let data_chunks = [vec![[10; 32], [20; 32], [30; 32]]];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<DataTransaction> = Vec::new();

    for chunks in data_chunks.iter() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        // we have to use a different signer so we get a unique txid for each transaction, despite the identical data_root
        let s = &signer2;

        // Get price from the API
        let price_info = node
            .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
            .await
            .expect("Failed to get price");

        let tx = s
            .create_publish_transaction(
                data,
                block1.block_hash,
                price_info.perm_fee.into(),
                price_info.term_fee.into(),
            )
            .unwrap();
        let tx = s.sign_transaction(tx).unwrap();
        println!("tx[2] {}", tx.header.id);
        txs.push(tx);
    }
    // submit tx 3
    // upload their chunks, make sure 3 is promoted

    // Post the tx & initialize some state to track their confirmation
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

    // Wait for all the transactions to be confirmed
    let result = node.wait_for_migrated_txs(unconfirmed_tx, 20).await;
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

    let block_tx1 = node
        .wait_for_block_containing_tx(txs[0].header.id, DataLedger::Publish, 60)
        .await?;
    // let block_tx2 = get_block_containing_tx(txs[2].header.id, Ledger::Publish, db).unwrap();

    let txid_1 = block_tx1.data_ledgers[DataLedger::Publish].tx_ids.0[0];
    //     let txid_2 = block_tx2.ledgers[Ledger::Publish].tx_ids.0[0];
    let first_tx_index: usize = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    println!("1:{:?}", block_tx1);
    //     println!("2:{}", block_tx2);

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

    // // Verify the chunks of the second promoted transaction
    // let tx_index = next_tx_index;

    // let chunk_offset = 3;
    // let expected_bytes = &data_chunks[tx_index][0];
    // verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    // let chunk_offset = 4;
    // let expected_bytes = &data_chunks[tx_index][1];
    // verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    // let chunk_offset = 5;
    // let expected_bytes = &data_chunks[tx_index][2];
    // verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    // println!("\n{:?}", unpacked_chunk);

    // Because it takes multiple ingress proofs to promote a tx, we keep ingress
    // proofs around until their data_roots expire out of the Submit ledger
    // (there was code to check if proofs expired here)

    node.stop().await;

    Ok(())
}
