use irys_api_server::{error::ApiError, routes, ApiState};
use irys_chain::chain::start_for_testing_default;

use actix_web::{
    middleware::Logger,
    test,
    web::{self, JsonConfig},
    App,
};
use awc::http::StatusCode;
use base58::ToBase58;
use serde::Serialize;


#[cfg(test)]
#[actix_web::test]
async fn api_end_to_end_test() {
    use std::{sync::Arc, time::Duration};
    use irys_types::{irys::IrysSigner, Base64, IrysTransactionHeader, PackedChunk, StorageConfig, UnpackedChunk, CHUNK_SIZE, MAX_CHUNK_SIZE};
    use rand::Rng;
    use tokio::time::sleep;
    use tracing::{debug, info};    

    let chunk_size: usize = CHUNK_SIZE as usize;

    let miner_signer = IrysSigner::random_signer_with_chunk_size(chunk_size);

    let storage_config = StorageConfig {
        chunk_size: chunk_size as u64, 
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: miner_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1_000,
        };


    let handle = start_for_testing_default(Some("api_end_to_end_test"), false, miner_signer, storage_config)
        .await
        .unwrap();
    handle.actor_addresses.start_mining().unwrap();

    let app_state = ApiState {
        db: handle.db,
        mempool: handle.actor_addresses.mempool,
        chunk_provider: Arc::new(handle.chunk_provider.clone()),
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

    // Create 2.5 chunks worth of data *  fill the data with random bytes
    let data_size = chunk_size * 2 as usize;
    let mut data_bytes = vec![0u8; data_size];
    rand::thread_rng().fill(&mut data_bytes[..]);

    // Create a new Irys API instance & a signed transaction
    let irys = IrysSigner::random_signer_with_chunk_size(chunk_size);
    let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
    let tx = irys.sign_transaction(tx).unwrap();

    // Make a POST request with JSON payload
    let req = test::TestRequest::post()
        .uri("/v1/tx")
        .set_json(&tx.header)
        .to_request();

    info!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

    // Call the service
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    info!("Transaction was posted");

    // Loop though each of the transaction chunks
    for (index, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[index].proof.to_vec());

        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(data_bytes[min..max].to_vec()),
            chunk_index: index as u32,
        };

        // Make a POST request with JSON payload
        let req = test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request();

        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        debug!("Response body: {:#?}", body);
        assert_eq!(status, StatusCode::OK);
    }
    let id: String = tx.header.id.as_bytes().to_base58();
    let mut attempts = 1;
    let max_attempts = 60;
    let delay = Duration::from_secs(1);

    // pools for tx being stored
    while attempts < max_attempts {
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let result: IrysTransactionHeader = test::read_body_json(resp).await;
            //assert_eq!(tx.header, result);
            info!("Transaction was retrived ok after {} attempts", attempts);            
            break;
        }

        attempts += 1;
        sleep(delay).await;
    }

    assert!(
        attempts < max_attempts,
        "Transaction was not stored in time"
    );

    attempts = 1;

    let mut missing_chunks = vec![1,0];
    let ledger = 1; // Submit ledger

    // pools for chunk being stored
    while attempts < max_attempts {
        let chunk = missing_chunks.pop().unwrap();
        info!("Retrieving chunk {}", chunk);
        let req = test::TestRequest::get()
        .uri(&format!("/v1/chunk/{}/{}", ledger, chunk))        
        .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let result: PackedChunk = test::read_body_json(resp).await;
            assert_eq!(chunk, result.chunk_index as usize, "Got different chunk index");
            // TODO: unpack the chunk and compare the data, now is missing partition chunk offset to unpack            
            // assert_eq!(Base64(data_bytes[chunk * chunk_size..(chunk + 1) * chunk_size].to_vec()), result.bytes, "Got different chunk data");
            info!("Chunk {} was retrived ok after {} attempts", chunk, attempts);            
            if missing_chunks.is_empty() {
                break;
            }
        } else {
            missing_chunks.push(chunk);
        }

        attempts += 1;
        sleep(delay).await;
    }

    assert!(
        attempts < max_attempts,
        "Chunk could not be retrieved in time"
    );

}
