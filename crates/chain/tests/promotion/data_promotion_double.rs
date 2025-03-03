use crate::utils::{mine_blocks, post_chunk};
use awc::http::StatusCode;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_database::Ledger;

use irys_types::Config;
use tracing::debug;

#[actix_web::test]
async fn serial_double_root_data_promotion_test() {
    use std::time::Duration;

    use actix_web::{
        middleware::Logger,
        test::{self, call_service, TestRequest},
        web::{self, JsonConfig},
        App,
    };
    use alloy_core::primitives::U256;
    use base58::ToBase58;
    use irys_actors::packing::wait_for_packing;
    use irys_api_server::{routes, ApiState};
    use irys_database::{tables::IngressProofs, walk_all};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        irys::IrysSigner, IrysTransaction, IrysTransactionHeader, LedgerChunkOffset, StorageConfig,
    };
    use reth_db::Database as _;
    use reth_primitives::GenesisAccount;
    use tokio::time::sleep;
    use tracing::info;

    use crate::utils::{get_block_parent, get_chunk, mine_block, verify_published_chunk};

    let chunk_size = 32; // 32 byte chunks
    let mut testnet_config = Config {
        chunk_size: chunk_size as u64,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        num_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        finalization_depth: 1, // Testnet / single node config
        ..Config::testnet()
    };
    testnet_config.chunk_size = chunk_size;

    let storage_config = StorageConfig::new(&testnet_config);

    let temp_dir = setup_tracing_and_temp_dir(Some("double_root_data_promotion_test"), false);
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let signer = IrysSigner::random_signer(&testnet_config);
    let signer2 = IrysSigner::random_signer(&testnet_config);

    config.extend_genesis_accounts(vec![
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

    // This will create 3 storage modules, one for submit, one for publish, and one for capacity
    let node_context = start_irys_node(
        config.clone(),
        storage_config.clone(),
        testnet_config.clone(),
    )
    .await
    .unwrap();

    wait_for_packing(
        node_context.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();

    let block1 = mine_block(&node_context).await.unwrap().unwrap();

    // node_context.actor_addresses.start_mining().unwrap();

    let app_state = ApiState {
        reth_provider: None,
        block_index: None,
        block_tree: None,
        db: node_context.db.clone(),
        mempool: node_context.actor_addresses.mempool.clone(),
        chunk_provider: node_context.chunk_provider.clone(),
        config: testnet_config,
    };

    // Initialize the app
    let app = test::init_service(
        App::new()
            .app_data(JsonConfig::default().limit(1024 * 1024)) // 1MB limit
            .app_data(web::Data::new(app_state))
            .wrap(Logger::default())
            .service(routes()),
    )
    .await;

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
    let delay = Duration::from_secs(1);
    for attempt in 1..20 {
        // Do we have any unconfirmed tx?
        let Some(tx) = unconfirmed_tx.first() else {
            // if not exit the loop.
            break;
        };

        // Attempt to retrieve the tx header from the HTTP endpoint
        let id: String = tx.id.as_bytes().to_base58();
        let resp = call_service(
            &app,
            TestRequest::get()
                .uri(&format!("/v1/tx/{}", id))
                .to_request(),
        )
        .await;

        if resp.status() == StatusCode::OK {
            let result: IrysTransactionHeader = test::read_body_json(resp).await;
            assert_eq!(*tx, result);
            info!("Transaction was retrieved ok after {} attempts", attempt);
            unconfirmed_tx.remove(0);
        }

        mine_block(&node_context).await.unwrap();
    }

    // Verify all transactions are confirmed
    assert_eq!(unconfirmed_tx.len(), 0);

    // ==============================
    // Post Tx chunks out of order
    // ------------------------------
    let _tx_index = 2;

    // // Last Tx, last chunk
    // let chunk_index = 2;
    // post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // // Last Tx, middle chunk
    // let chunk_index = 1;
    // post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

    // // Last Tx, first chunk
    // let chunk_index = 0;
    // post_chunk(&app, &txs[tx_index], chunk_index, &data_chunks[tx_index]).await;

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
    let mut unconfirmed_promotions = vec![
        // txs[2].header.id.as_bytes().to_base58(),
        txs[0].header.id.as_bytes().to_base58(),
    ];
    println!("unconfirmed_promotions: {:?}", unconfirmed_promotions);

    for attempts in 1..20 {
        // Do we have any unconfirmed promotions?
        let Some(txid) = unconfirmed_promotions.first() else {
            // if not exit the loop.
            break;
        };

        // Attempt to retrieve the transactions from the node endpoint
        println!("Attempting... {}", txid);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &txid))
            .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let tx_header: IrysTransactionHeader = test::read_body_json(resp).await;
            info!("Transaction was retrieved ok after {} attempts", attempts);
            if let Some(_proof) = tx_header.ingress_proofs {
                assert_eq!(tx_header.id.as_bytes().to_base58(), *txid);
                println!("Confirming... {}", tx_header.id.as_bytes().to_base58());
                unconfirmed_promotions.remove(0);
                println!("unconfirmed_promotions: {:?}", unconfirmed_promotions);
            }
        }
        mine_block(&node_context).await.unwrap();
        sleep(delay).await;
    }

    assert_eq!(unconfirmed_promotions.len(), 0);

    // wait for the first set of chunks chunk to appear in the publish ledger
    for _attempts in 1..20 {
        if let Some(_packed_chunk) =
            get_chunk(&app, Ledger::Publish, LedgerChunkOffset::from(0)).await
        {
            println!("First set of chunks found!");
            break;
        }
        sleep(delay).await;
    }

    // wait for the second set of chunks to appear in the publish ledger
    for _attempts in 1..20 {
        if let Some(_packed_chunk) =
            get_chunk(&app, Ledger::Publish, LedgerChunkOffset::from(3)).await
        {
            println!("Second set of chunks found!");
            break;
        }
        sleep(delay).await;
    }

    let db = &node_context.db.clone();
    let block_tx1 = get_block_parent(txs[0].header.id, Ledger::Publish, db).unwrap();
    // let block_tx2 = get_block_parent(txs[2].header.id, Ledger::Publish, db).unwrap();

    let first_tx_index: usize;
    let _next_tx_index: usize;

    // if block_tx1.block_hash == block_tx2.block_hash {
    //     // Extract the transaction order
    //     let txid_1 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[0];
    //     // let txid_2 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[1];
    //     first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     // next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    //     println!("1:{}", block_tx1);
    // } else if block_tx1.height > block_tx2.height {
    //     let txid_1 = block_tx2.ledgers[Ledger::Publish].tx_ids.0[0];
    //     let txid_2 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[0];
    //     first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    //     println!("1:{}", block_tx2);
    //     println!("2:{}", block_tx1);
    // } else {
    //     let txid_1 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[0];
    //     let txid_2 = block_tx2.ledgers[Ledger::Publish].tx_ids.0[0];
    //     first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    //     println!("1:{}", block_tx1);
    //     println!("2:{}", block_tx2);
    // }

    let txid_1 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[0];
    //     let txid_2 = block_tx2.ledgers[Ledger::Publish].tx_ids.0[0];
    first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    println!("1:{}", block_tx1);
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
        &storage_config,
    )
    .await;

    let chunk_offset = 1;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &storage_config,
    )
    .await;

    let chunk_offset = 2;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &storage_config,
    )
    .await;

    // Part 2
    debug!("PHASE 2");

    // mine 1 block
    let blk = mine_block(&node_context).await.unwrap().unwrap();
    debug!("P2 block {}", &blk.0.height);

    // ensure the ingress proof still exists
    let ingress_proofs = db
        .view(|rtx| walk_all::<IngressProofs, _>(rtx))
        .unwrap()
        .unwrap();
    assert_eq!(ingress_proofs.len(), 1);

    // same chunks as tx1
    let data_chunks = [vec![[10; 32], [20; 32], [30; 32]]];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them

    let mut txs: Vec<IrysTransaction> = Vec::new();

    for (_i, chunks) in data_chunks.iter().enumerate() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        // we have to use a different signer so we get a unique txid for each transaction, despite the identical data_root
        let s = &signer2;
        let tx = s
            .create_transaction(data, Some(block1.0.block_hash))
            .unwrap();
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
    let delay = Duration::from_secs(1);
    for attempt in 1..20 {
        // Do we have any unconfirmed tx?
        let Some(tx) = unconfirmed_tx.first() else {
            // if not exit the loop.
            break;
        };

        // Attempt to retrieve the tx header from the HTTP endpoint
        let id: String = tx.id.as_bytes().to_base58();
        let resp = call_service(
            &app,
            TestRequest::get()
                .uri(&format!("/v1/tx/{}", id))
                .to_request(),
        )
        .await;

        if resp.status() == StatusCode::OK {
            let result: IrysTransactionHeader = test::read_body_json(resp).await;
            assert_eq!(*tx, result);
            info!("Transaction was retrieved ok after {} attempts", attempt);
            unconfirmed_tx.remove(0);
        }

        mine_blocks(&node_context, 1).await.unwrap();
    }

    // Verify all transactions are confirmed
    assert_eq!(unconfirmed_tx.len(), 0);

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
    let mut unconfirmed_promotions = vec![
        // txs[2].header.id.as_bytes().to_base58(),
        txs[0].header.id.as_bytes().to_base58(),
    ];
    println!("unconfirmed_promotions: {:?}", unconfirmed_promotions);

    for attempts in 1..20 {
        // Do we have any unconfirmed promotions?
        let Some(txid) = unconfirmed_promotions.first() else {
            // if not exit the loop.
            break;
        };

        // Attempt to retrieve the transactions from the node endpoint
        println!("Attempting... {}", txid);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &txid))
            .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let tx_header: IrysTransactionHeader = test::read_body_json(resp).await;
            info!("Transaction was retrieved ok after {} attempts", attempts);
            if let Some(_proof) = tx_header.ingress_proofs {
                assert_eq!(tx_header.id.as_bytes().to_base58(), *txid);
                println!("Confirming... {}", tx_header.id.as_bytes().to_base58());
                unconfirmed_promotions.remove(0);
                println!("unconfirmed_promotions: {:?}", unconfirmed_promotions);
            }
        }
        mine_blocks(&node_context, 1).await.unwrap();
        sleep(delay).await;
    }

    assert_eq!(unconfirmed_promotions.len(), 0);

    // wait for the second set of chunks to appear in the publish ledger
    for _attempts in 1..20 {
        if let Some(_packed_chunk) =
            get_chunk(&app, Ledger::Publish, LedgerChunkOffset::from(3)).await
        {
            println!("Second set of chunks found!");
            break;
        }
        sleep(delay).await;
    }

    let db = &node_context.db.clone();
    let block_tx1 = get_block_parent(txs[0].header.id, Ledger::Publish, db).unwrap();
    // let block_tx2 = get_block_parent(txs[2].header.id, Ledger::Publish, db).unwrap();

    let first_tx_index: usize;

    let txid_1 = block_tx1.ledgers[Ledger::Publish].tx_ids.0[0];
    //     let txid_2 = block_tx2.ledgers[Ledger::Publish].tx_ids.0[0];
    first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
    //     next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
    println!("1:{}", block_tx1);
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
        &storage_config,
    )
    .await;

    let chunk_offset = 1;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &storage_config,
    )
    .await;

    let chunk_offset = 2;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(
        &app,
        LedgerChunkOffset::from(chunk_offset),
        expected_bytes,
        &storage_config,
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

    mine_blocks(&node_context, 5).await.unwrap();
    // ensure the ingress proof is gone
    let ingress_proofs = db
        .view(|rtx| walk_all::<IngressProofs, _>(rtx))
        .unwrap()
        .unwrap();
    assert_eq!(ingress_proofs.len(), 0);
}
