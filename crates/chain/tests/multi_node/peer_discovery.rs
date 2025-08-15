use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::utils::IrysNodeTest;
use actix_web::test::{call_service, read_body, TestRequest};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_actors::packing::wait_for_packing;
use irys_types::{
    build_user_agent, irys::IrysSigner, NodeConfig, PeerAddress, PeerResponse, RethPeerInfo,
    VersionRequest,
};
use tracing::{debug, error};

#[test_log::test(actix_web::test)]
async fn heavy_peer_discovery() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.trusted_peers = vec![];
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().block_migration_depth = 1;
    config.storage.num_writes_before_sync = 1;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone());
    let node = node.start().await;
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    node.node_ctx.start_mining().await.unwrap();

    let app = node.start_public_api().await;

    // Invoke the peer list endpoint
    let req = TestRequest::get().uri("/v1/peer_list").to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_list: Vec<PeerAddress> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("Parsed JSON: {:?}", peer_list);

    // Post a 3 peer requests from different mining addresses, have them report
    // different IP addresses
    let miner_signer_1 = IrysSigner::random_signer(&config.consensus_config());
    let mut version_request = VersionRequest {
        chain_id: miner_signer_1.chain_id,
        address: PeerAddress {
            gossip: "127.0.0.1:8080".parse().expect("valid socket address"),
            api: "127.0.0.1:8081".parse().expect("valid socket address"),
            execution: RethPeerInfo {
                peering_tcp_addr: "127.0.0.1:8082".parse().unwrap(),
                peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            },
        },
        mining_address: miner_signer_1.address(),
        user_agent: Some(build_user_agent("miner1", "0.1.0")),
        ..Default::default()
    };
    miner_signer_1
        .sign_p2p_handshake(&mut version_request)
        .expect("sign p2p handshake");

    let req = TestRequest::post()
        .uri("/v1/version")
        .peer_addr("127.0.0.1:12345".parse().unwrap())
        .set_json(&version_request)
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

    // Test that we get rejected if the source IP doesn't match the gossip IP
    let req = TestRequest::post()
        .uri("/v1/version")
        .peer_addr("127.0.0.5:12345".parse().unwrap())
        .set_json(&version_request)
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");

    match peer_response {
        PeerResponse::Accepted(_) => panic!("Expected Rejected response, got Accepted"),
        PeerResponse::Rejected(response) => {
            assert!(matches!(
                response.reason,
                irys_types::RejectionReason::InvalidCredentials
            ));
            assert_eq!(
                response.message,
                Some("The source address does not match the request address".to_string())
            );
            assert!(response.retry_after.is_none(), "Expected no retry after");
        }
    }

    let miner_signer_2 = IrysSigner::random_signer(&config.consensus_config());

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let version_json = serde_json::json!({
        "version": "0.1.0",
        "protocol_version": "V1",
        "chain_id": 1270,
        "address": {
            "gossip": "127.0.0.2:8080",
            "api": "127.0.0.2:8081",
            "execution": {
                "peering_tcp_addr": "127.0.0.2:8082",
                "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
            },
        },
        "mining_address": "0x050e7a06903a4a7af956efc2842d224775e52b59",
        "user_agent": "miner2/0.1.0 (macos/aarch64)",
        "timestamp": 0,
        "signature": "6npXVuBZ7oCyjPUPamZikc3txKCFvmhM3GX9yWDmBQ4dtbdjSmtqNsq6DpGegiw8ENkfkZ1K797L2VHCeb6rfkiFt"
    });

    let req = TestRequest::post()
        .uri("/v1/version")
        .peer_addr("127.0.0.2:12345".parse().unwrap())
        .set_json(&version_json) // Pass the JSON value directly
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    debug!("\nParsed Response:");
    debug!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        PeerResponse::Accepted(accepted) => {
            assert!(!accepted.peers.is_empty(), "Expected at least 1 peers");
            debug!("Accepted peers: {:?}", accepted.peers);
            assert!(
                accepted.peers.contains(&PeerAddress {
                    gossip: "127.0.0.1:8080".parse().unwrap(),
                    api: "127.0.0.1:8081".parse().unwrap(),
                    execution: RethPeerInfo {
                        peering_tcp_addr: "127.0.0.1:8082".parse().unwrap(),
                        peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                    },
                }),
                "Missing expected peer 127.0.0.1:8080"
            );
        }
        PeerResponse::Rejected(rejected) => {
            panic!("Expected Accepted response, got {:?}", rejected)
        }
    }

    let version_json = serde_json::json!({
        "version": "0.1.0",
        "protocol_version": "V1",
        "chain_id": miner_signer_2.chain_id,
        "address": {
            "gossip": "127.0.0.2:8080",
            "api": "127.0.0.2:8081",
            "execution": {
                "peering_tcp_addr": "127.0.0.2:8082",
                "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
            },
        },
        "mining_address": miner_signer_2.address(),
        "user_agent": build_user_agent("miner2", "0.1.0"),
        "timestamp": timestamp,
        // Signature from another signer, should fail verification
        "signature": "7vAD7AoznW7zzFuyxnT4ghhX3i7jAZbR3i2tt8Pe8L6nCdNpDJHFA4N5qEvRMNyvkUHEDZShiXzjLniBet6rrPwtN"
    });

    let req = TestRequest::post()
        .uri("/v1/version")
        .peer_addr("127.0.0.2:12345".parse().unwrap())
        .set_json(&version_json) // Pass the JSON value directly
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: PeerResponse =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    debug!("\nParsed Response:");
    debug!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        PeerResponse::Accepted(_) => panic!("Expected Rejected response, got Accepted"),
        PeerResponse::Rejected(response) => {
            assert!(matches!(
                response.reason,
                irys_types::RejectionReason::InvalidCredentials
            ));
            assert_eq!(
                response.message,
                Some("Signature verification failed".to_string())
            );
            assert!(response.retry_after.is_none(), "Expected no retry after");
        }
    }

    let miner_signer_3 = IrysSigner::random_signer(&config.consensus_config());
    let mut version_request = VersionRequest {
        chain_id: miner_signer_3.chain_id,
        address: PeerAddress {
            gossip: "127.0.0.3:8080".parse().expect("valid socket address"),
            api: "127.0.0.3:8081".parse().expect("valid socket address"),
            execution: RethPeerInfo {
                peering_tcp_addr: "127.0.0.3:8082".parse().unwrap(),
                peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            },
        },
        mining_address: miner_signer_3.address(),
        user_agent: Some(build_user_agent("miner3", "0.1.0")),
        ..Default::default()
    };
    miner_signer_3
        .sign_p2p_handshake(&mut version_request)
        .expect("sign p2p handshake");

    let req = TestRequest::post()
        .uri("/v1/version")
        .peer_addr("127.0.0.3:12345".parse().unwrap())
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
            assert!(accepted.peers.len() >= 2, "Expected at least 2 peers");
            assert!(
                accepted.peers.contains(&PeerAddress {
                    gossip: "127.0.0.1:8080".parse().unwrap(),
                    api: "127.0.0.1:8081".parse().unwrap(),
                    execution: RethPeerInfo {
                        peering_tcp_addr: "127.0.0.1:8082".parse().unwrap(),
                        peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                    },
                }),
                "Missing expected peer 127.0.0.1:8080"
            );
            assert!(
                accepted.peers.contains(&PeerAddress {
                    gossip: "127.0.0.2:8080".parse().unwrap(),
                    api: "127.0.0.2:8081".parse().unwrap(),
                    execution: RethPeerInfo {
                        peering_tcp_addr: "127.0.0.2:8082".parse().unwrap(),
                        peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                    },
                }),
                "Missing expected peer 127.0.0.2:8080"
            );
        }
        PeerResponse::Rejected(_) => panic!("Expected Accepted response, got Rejected"),
    }

    // Verify the peer_list shows all the peers
    let req = TestRequest::get()
        .uri("/v1/peer_list")
        .peer_addr("127.0.0.2:12345".parse().unwrap())
        .to_request();
    let resp = call_service(&app, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_list: Vec<PeerAddress> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("Parsed JSON: {:?}", peer_list);
    assert!(
        vec![
            PeerAddress {
                gossip: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
                api: "127.0.0.1:8081".parse::<SocketAddr>().unwrap(),
                execution: RethPeerInfo {
                    peering_tcp_addr: "127.0.0.1:8082".parse().unwrap(),
                    peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                },
            },
            PeerAddress {
                gossip: "127.0.0.2:8080".parse::<SocketAddr>().unwrap(),
                api: "127.0.0.2:8081".parse::<SocketAddr>().unwrap(),
                execution: RethPeerInfo {
                    peering_tcp_addr: "127.0.0.2:8082".parse().unwrap(),
                    peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                },
            },
            PeerAddress {
                gossip: "127.0.0.3:8080".parse::<SocketAddr>().unwrap(),
                api: "127.0.0.3:8081".parse::<SocketAddr>().unwrap(),
                execution: RethPeerInfo {
                    peering_tcp_addr: "127.0.0.3:8082".parse().unwrap(),
                    peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
                },
            },
        ]
        .iter()
        .all(|addr| {
            let contains = peer_list.contains(addr);

            if !contains {
                error!("Missing expected peer {:?}", addr);
            }

            contains
        }),
        "Peer list missing expected addresses"
    );

    node.stop().await;
    Ok(())
}
