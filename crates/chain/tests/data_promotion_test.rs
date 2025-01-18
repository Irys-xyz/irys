use std::collections::HashMap;

use actix_web::{
    dev::{Service, ServiceResponse},
    test,
};
use awc::{body::MessageBody, http::StatusCode};
use irys_database::{tables::IrysBlockHeaders, Ledger};
use irys_packing::unpack;
use irys_types::{
    Base64, DatabaseProvider, IrysBlockHeader, IrysTransaction, LedgerChunkOffset, PackedChunk,
    StorageConfig, UnpackedChunk, H256,
};
use reth_db::cursor::*;
use reth_db::Database;
use tracing::{debug, error};

#[cfg(test)]
#[actix_web::test]
async fn data_promotion_test() {
    use std::{sync::Arc, time::Duration};

    use actix_web::{
        middleware::Logger,
        test::{self, call_service, TestRequest},
        web::{self, JsonConfig},
        App,
    };
    use base58::ToBase58;
    use irys_api_server::{routes, ApiState};
    use irys_chain::start_for_testing_default;
    use irys_types::{irys::IrysSigner, IrysTransaction, IrysTransactionHeader, StorageConfig};
    use tokio::time::sleep;
    use tracing::info;

    let chunk_size = 32; // 32Byte chunks

    let miner_signer = IrysSigner::random_signer_with_chunk_size(chunk_size);

    let storage_config = StorageConfig {
        chunk_size: chunk_size as u64,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: miner_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        num_confirmations_for_finality: 1, // Testnet / single node config
    };

    // This will create 3 storage modules, one for submit, one for publish, and one for capacity
    let node_context = start_for_testing_default(
        Some("data_promotion_test"),
        false,
        miner_signer,
        storage_config.clone(),
    )
    .await
    .unwrap();
    node_context.actor_addresses.start_mining().unwrap();

    let app_state = ApiState {
        db: node_context.db.clone(),
        mempool: node_context.actor_addresses.mempool,
        chunk_provider: Arc::new(node_context.chunk_provider.clone()),
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
        vec![[70; 32], [80; 32], [90; 32]],
    ];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them
    let signer = IrysSigner::random_signer_with_chunk_size(chunk_size as usize);
    let mut txs: Vec<IrysTransaction> = Vec::new();

    for (i, chunks) in data_chunks.iter().enumerate() {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(chunk);
        }
        let tx = signer.create_transaction(data, None).unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        println!("tx[{}] {}", i, tx.header.id.as_bytes().to_base58());
        txs.push(tx);
    }

    // Post the 3 transactions & initialize some state to track their confirmation
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

        sleep(delay).await;
    }

    // Verify all transactions are confirmed
    assert_eq!(unconfirmed_tx.len(), 0);

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
    let mut unconfirmed_promotions = vec![
        txs[2].header.id.as_bytes().to_base58(),
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

        sleep(delay).await;
    }

    assert_eq!(unconfirmed_promotions.len(), 0);

    // wait for the first set of chunks chunk to appear in the publish ledger
    for _attempts in 1..20 {
        if let Some(_packed_chunk) = get_chunk(&app, Ledger::Publish, 0).await {
            println!("First set of chunks found!");
            break;
        }
        sleep(delay).await;
    }

    // wait for the second set of chunks to appear in the publish ledger
    for _attempts in 1..20 {
        if let Some(_packed_chunk) = get_chunk(&app, Ledger::Publish, 3).await {
            println!("Second set of chunks found!");
            break;
        }
        sleep(delay).await;
    }

    let db = &node_context.db.clone();
    let block_tx1 = get_block_parent(txs[0].header.id, Ledger::Publish, db).unwrap();
    let block_tx2 = get_block_parent(txs[2].header.id, Ledger::Publish, db).unwrap();

    let first_tx_index: usize;
    let next_tx_index: usize;

    if block_tx1.block_hash == block_tx2.block_hash {
        // Extract the transaction order
        let txid_1 = block_tx1.ledgers[Ledger::Publish].txids.0[0];
        let txid_2 = block_tx1.ledgers[Ledger::Publish].txids.0[1];
        first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
        next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
        println!("1:{}", block_tx1);
    } else {
        if block_tx1.height > block_tx2.height {
            let txid_1 = block_tx2.ledgers[Ledger::Publish].txids.0[0];
            let txid_2 = block_tx1.ledgers[Ledger::Publish].txids.0[0];
            first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
            next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
            println!("1:{}", block_tx2);
            println!("2:{}", block_tx1);
        } else {
            let txid_1 = block_tx1.ledgers[Ledger::Publish].txids.0[0];
            let txid_2 = block_tx2.ledgers[Ledger::Publish].txids.0[0];
            first_tx_index = txs.iter().position(|tx| tx.header.id == txid_1).unwrap();
            next_tx_index = txs.iter().position(|tx| tx.header.id == txid_2).unwrap();
            println!("1:{}", block_tx1);
            println!("2:{}", block_tx2);
        }
    }

    // ==============================
    // Verify chunk ordering in publish ledger storage module
    // ------------------------------
    // Verify the chunks of the first promoted transaction
    let tx_index = first_tx_index;

    let chunk_offset = 0;
    let expected_bytes = &data_chunks[tx_index][0];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    let chunk_offset = 1;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    let chunk_offset = 2;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    // Verify the chunks of the second promoted transaction
    let tx_index = next_tx_index;

    let chunk_offset = 3;
    let expected_bytes = &data_chunks[tx_index][0];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    let chunk_offset = 4;
    let expected_bytes = &data_chunks[tx_index][1];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    let chunk_offset = 5;
    let expected_bytes = &data_chunks[tx_index][2];
    verify_published_chunk(&app, chunk_offset, expected_bytes, &storage_config).await;

    // println!("\n{:?}", unpacked_chunk);
}

/// Verifies that a published chunk matches its expected content.
/// Gets a chunk from storage, unpacks it, and compares against expected bytes.
/// Panics if the chunk is not found or content doesn't match expectations.
async fn verify_published_chunk<T, B>(
    app: &T,
    chunk_offset: LedgerChunkOffset,
    expected_bytes: &[u8; 32],
    storage_config: &StorageConfig,
) where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    if let Some(packed_chunk) = get_chunk(&app, Ledger::Publish, chunk_offset).await {
        let unpacked_chunk = unpack(
            &packed_chunk,
            storage_config.entropy_packing_iterations,
            storage_config.chunk_size as usize,
        );
        if unpacked_chunk.bytes.0 != expected_bytes {
            println!(
                "ledger_chunk_offset: {}\nfound: {:?}\nexpected: {:?}",
                chunk_offset, unpacked_chunk.bytes.0, expected_bytes
            )
        }
        assert_eq!(unpacked_chunk.bytes.0, expected_bytes);
    } else {
        panic!(
            "Chunk not found! Publish ledger chunk_offset: {}",
            chunk_offset
        );
    }
}

/// Helper function for testing chunk uploads. Posts a single chunk of transaction data
/// to the /v1/chunk endpoint and verifies successful response.
async fn post_chunk<T, B>(app: &T, tx: &IrysTransaction, chunk_index: usize, chunks: &Vec<[u8; 32]>)
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
{
    let chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(tx.proofs[chunk_index].proof.to_vec()),
        bytes: Base64(chunks[chunk_index].to_vec()),
        tx_offset: chunk_index as u32,
    };

    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
}

/// Retrieves a ledger chunk via HTTP GET request using the actix-web test framework.
///
/// # Arguments
/// * `app` - The actix-web service
/// * `ledger` - Target ledger
/// * `chunk_offset` - Ledger relative chunk offset
///
/// Returns `Some(PackedChunk)` if found (HTTP 200), `None` otherwise.
async fn get_chunk<T, B>(
    app: &T,
    ledger: Ledger,
    chunk_offset: LedgerChunkOffset,
) -> Option<PackedChunk>
where
    T: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let req = test::TestRequest::get()
        .uri(&format!(
            "/v1/chunk/ledger/{}/{}",
            ledger as usize, chunk_offset
        ))
        .to_request();

    let res = test::call_service(&app, req).await;

    if res.status() == StatusCode::OK {
        let packed_chunk: PackedChunk = test::read_body_json(res).await;
        Some(packed_chunk)
    } else {
        None
    }
}

/// Finds and returns the parent block header containing a given transaction ID.
/// Takes a transaction ID, ledger type, and database connection.
/// Returns None if the transaction isn't found in any block.
fn get_block_parent(txid: H256, ledger: Ledger, db: &DatabaseProvider) -> Option<IrysBlockHeader> {
    let read_tx = db
        .tx()
        .map_err(|e| {
            error!("Failed to create transaction: {}", e);
        })
        .ok()?;

    let mut read_cursor = read_tx
        .new_cursor::<IrysBlockHeaders>()
        .map_err(|e| {
            error!("Failed to create cursor: {}", e);
        })
        .ok()?;

    let walker = read_cursor
        .walk(None)
        .map_err(|e| {
            error!("Failed to create walker: {}", e);
        })
        .ok()?;

    let block_headers = walker
        .collect::<Result<HashMap<_, _>, _>>()
        .map_err(|e| {
            error!("Failed to collect results: {}", e);
        })
        .ok()?;

    // Loop tough all the blocks and find the one that contains the txid
    for block_header in block_headers.values() {
        if block_header.ledgers[ledger].txids.0.contains(&txid) {
            return Some(IrysBlockHeader::from(block_header.clone()));
        }
    }

    None
}
