// todo delete the whole module. the tests are ignored anyway. They can be restored in the future

use actix_http::StatusCode;
use alloy_core::primitives::U256;
use irys_actors::packing::wait_for_packing;
use irys_api_server::{routes, ApiState};
use irys_config::IrysNodeConfig;
use irys_packing::{unpack, PackingType, PACKING_TYPE};

use actix_web::{
    middleware::Logger,
    test,
    web::{self, JsonConfig},
    App,
};
use base58::ToBase58;
use irys_types::{Config, TxChunkOffset};
use reth_primitives::GenesisAccount;
use tracing::info;

use crate::utils::start_node_config;

#[actix_web::test]
async fn heavy_api_end_to_end_test_32b() {
    if PACKING_TYPE == PackingType::CPU {
        api_end_to_end_test(32).await;
    } else {
        info!("C packing implementation do  not support chunk size different from CHUNK_SIZE");
    }
}

#[actix_web::test]
async fn heavy_api_end_to_end_test_256kb() {
    api_end_to_end_test(256 * 1024).await;
}

async fn api_end_to_end_test(chunk_size: usize) {
    use irys_types::{
        irys::IrysSigner, Base64, IrysTransactionHeader, PackedChunk, UnpackedChunk,
    };
    use rand::Rng;
    use std::time::Duration;
    use tokio::time::sleep;
    use tracing::{debug, info};
    let entropy_packing_iterations = 1_000;
    let testnet_config = Config {
        chunk_size: chunk_size.try_into().unwrap(),
        entropy_packing_iterations,
        ..Config::testnet()
    };
    let chain_id = testnet_config.chain_id;
    let mut node_config = IrysNodeConfig::new(&testnet_config);

    let irys = IrysSigner::random_signer(&testnet_config);
    node_config.extend_genesis_accounts(vec![(
        irys.address(),
        GenesisAccount {
            balance: U256::from(1000),
            ..Default::default()
        },
    )]);

    let (handle, _tmp_dir) = start_node_config(
        &format!("api_end_to_end_{}_test_", chunk_size),
        Some(testnet_config.clone()),
        Some(node_config),
    )
    .await;

    handle.actor_addresses.start_mining().unwrap();

    let app_state = ApiState {
        reth_provider: None,
        reth_http_url: None,
        block_index: None,
        block_tree: None,
        db: handle.db,
        mempool: handle.actor_addresses.mempool,
        chunk_provider: handle.chunk_provider.clone(),
        config: testnet_config.clone(),
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

    wait_for_packing(
        handle.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await
    .unwrap();

    // Create 2.5 chunks worth of data *  fill the data with random bytes
    let data_size = chunk_size * 2_usize;
    let mut data_bytes = vec![0u8; data_size];
    rand::thread_rng().fill(&mut data_bytes[..]);

    // Create a new Irys API instance & a signed transaction

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
    let status = resp.status();
    let body = test::read_body(resp).await;
    debug!("Response body: {:#?}", body);
    assert_eq!(status, StatusCode::OK);
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
            tx_offset: TxChunkOffset::from(index as u32),
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
    let max_attempts = 20;

    let delay = Duration::from_secs(1);

    // polls for tx being stored
    while attempts < max_attempts {
        let req = test::TestRequest::get()
            .uri(&format!("/v1/tx/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let result: IrysTransactionHeader = test::read_body_json(resp).await;
            assert_eq!(tx.header, result);
            info!("Transaction was retrieved ok after {} attempts", attempts);
            break;
        }

        attempts += 1;
        sleep(delay).await;
    }

    assert!(
        attempts < max_attempts,
        "Transaction was not stored in after {} attempts",
        attempts
    );

    attempts = 1;

    let mut missing_chunks = vec![1, 0];
    let ledger = 1; // Submit ledger

    // polls for chunk being available
    while attempts < max_attempts {
        let chunk = missing_chunks.pop().unwrap();
        info!("Retrieving chunk: {} attempt: {}", chunk, attempts);
        let req = test::TestRequest::get()
            .uri(&format!("/v1/chunk/ledger/{}/{}", ledger, chunk))
            .to_request();

        let resp = test::call_service(&app, req).await;

        if resp.status() == StatusCode::OK {
            let packed_chunk: PackedChunk = test::read_body_json(resp).await;
            assert_eq!(
                chunk, *packed_chunk.tx_offset as usize,
                "Got different chunk index"
            );

            let unpacked_chunk = unpack(
                &packed_chunk,
                entropy_packing_iterations,
                chunk_size,
                chain_id,
            );
            assert_eq!(
                unpacked_chunk.bytes.0,
                data_bytes[chunk * chunk_size..(chunk + 1) * chunk_size],
                "Got different chunk data"
            );
            info!(
                "Chunk {} was retrieved ok after {} attempts",
                chunk, attempts
            );
            attempts = 0;
            if missing_chunks.is_empty() {
                break;
            }
        } else {
            let body = test::read_body(resp).await;
            debug!("Chunk not available. Response body: {:#?}", body);
            missing_chunks.push(chunk);
        }

        attempts += 1;
        sleep(delay).await;
    }

    assert!(
        attempts < max_attempts,
        "Chunk could not be retrieved after {} attempts",
        attempts
    );
}
