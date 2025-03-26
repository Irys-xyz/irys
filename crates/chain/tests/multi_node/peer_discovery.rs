use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use actix_web::{
    middleware::Logger,
    test::{self, call_service, read_body, TestRequest},
    web::{self, JsonConfig},
    App,
};
use alloy_core::primitives::U256;
use irys_actors::packing::wait_for_packing;
use irys_api_server::{routes, ApiState};
use irys_chain::start_irys_node;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::Config;
use irys_types::{build_user_agent, irys::IrysSigner, PeerResponse, VersionRequest};
use reth_primitives::GenesisAccount;

#[actix_web::test]
async fn serial_peer_discovery() -> eyre::Result<()> {
    let chunk_size = 32; // 32Byte chunks

    let mut test_config = Config::testnet();

    // Override testnet parameters for local test
    test_config.chunk_size = chunk_size as u64;
    test_config.num_chunks_in_partition = 10;
    test_config.num_chunks_in_recall_range = 2;
    test_config.num_partitions_per_slot = 1;
    test_config.num_writes_before_sync = 1;
    test_config.entropy_packing_iterations = 1_000;
    test_config.chunk_migration_depth = 1;

    let temp_dir = setup_tracing_and_temp_dir(Some("data_promotion_test"), false);
    let mut config = irys_config::IrysNodeConfig {
        base_directory: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let signer = IrysSigner::random_signer(&test_config);

    config.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);

    // This will create 3 storage modules, one for submit, one for publish, and one for capacity
    let storage_config = irys_types::StorageConfig::new(&test_config);
    let node = start_irys_node(config, storage_config, test_config.clone()).await?;
    wait_for_packing(
        node.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    node.actor_addresses.start_mining().unwrap();

    let app_state = ApiState {
        reth_provider: None,
        reth_http_url: None,
        block_index: None,
        block_tree: None,
        db: node.db.clone(),
        mempool: node.actor_addresses.mempool.clone(),
        chunk_provider: node.chunk_provider.clone(),
        config: test_config.clone(),
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

    // Invoke the peer list endpoint
    let req = TestRequest::get().uri("/v1/peer_list").to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_list: Vec<SocketAddr> = serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("Parsed JSON: {:?}", peer_list);

    // Now you can work with the structured data
    assert!(peer_list.is_empty(), "Peer list should be empty");

    // Post a 3 peer requests from different mining addresses, have them report
    // different IP addresses
    let miner_signer_1 = IrysSigner::random_signer(&test_config);
    let version_request = VersionRequest {
        mining_address: miner_signer_1.address(),
        chain_id: miner_signer_1.chain_id,
        address: "127.0.0.1:8080".parse().expect("valid socket address"),
        user_agent: Some(build_user_agent("miner1", "0.1.0")),
        ..Default::default()
    };

    let req = TestRequest::post()
        .uri("/v1/version")
        .set_json(version_request)
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");

    // Convert to pretty JSON string
    let pretty_json =
        serde_json::to_string_pretty(&peer_response).expect("Failed to serialize to pretty JSON");
    println!("Pretty JSON:\n{}", pretty_json);

    let miner_signer_2 = IrysSigner::random_signer(&test_config);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Example of Define the JSON directly like you would from javascript
    let version_json = serde_json::json!({
        "version": "0.1.0",
        "protocol_version": "V1",
        "mining_address": miner_signer_2.address(),
        "chain_id": miner_signer_2.chain_id,
        "address": "127.0.0.2:8080",
        "user_agent": build_user_agent("miner2", "0.1.0"),
        "timestamp": timestamp
    });

    let req = TestRequest::post()
        .uri("/v1/version")
        .set_json(&version_json) // Pass the JSON value directly
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("\nParsed Response:");
    println!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        PeerResponse::Accepted(accepted) => {
            assert_eq!(accepted.peers.len(), 1, "Expected 1 peers");
            assert!(
                accepted.peers.contains(&"127.0.0.1:8080".parse().unwrap()),
                "Missing expected peer 127.0.0.1:8080"
            );
        }
        PeerResponse::Rejected(_) => panic!("Expected Accepted response, got Rejected"),
    }

    let miner_signer_3 = IrysSigner::random_signer(&test_config);
    let version_request = VersionRequest {
        mining_address: miner_signer_3.address(),
        chain_id: miner_signer_3.chain_id,
        address: "127.0.0.3:8080".parse().expect("valid socket address"),
        user_agent: Some(build_user_agent("miner3", "0.1.0")),
        ..Default::default()
    };

    let req = TestRequest::post()
        .uri("/v1/version")
        .set_json(version_request)
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("\nParsed Response:");
    println!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        PeerResponse::Accepted(accepted) => {
            assert_eq!(accepted.peers.len(), 2, "Expected 2 peers");
            assert!(
                accepted.peers.contains(&"127.0.0.1:8080".parse().unwrap()),
                "Missing expected peer 127.0.0.1:8080"
            );
            assert!(
                accepted.peers.contains(&"127.0.0.2:8080".parse().unwrap()),
                "Missing expected peer 127.0.0.2:8080"
            );
        }
        PeerResponse::Rejected(_) => panic!("Expected Accepted response, got Rejected"),
    }

    // Verify the peer_list shows all the peers
    let req = TestRequest::get().uri("/v1/peer_list").to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_list: Vec<SocketAddr> = serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("Parsed JSON: {:?}", peer_list);

    assert!(
        peer_list.iter().all(|addr| {
            vec![
                "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
                "127.0.0.2:8080".parse::<SocketAddr>().unwrap(),
                "127.0.0.3:8080".parse::<SocketAddr>().unwrap(),
            ]
            .contains(addr)
        }),
        "Peer list missing expected addresses"
    );

    node.stop().await;
    Ok(())
}
