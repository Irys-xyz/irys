use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::utils::IrysNodeTest;
use actix_web::test::{call_service, read_body, TestRequest};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;

use irys_domain::ScoreDecreaseReason;
use irys_p2p::{GossipResponse, GossipRoutes, RejectionReason};
use irys_types::{
    build_user_agent, irys::IrysSigner, BlockHash, HandshakeRequestV2, HandshakeResponse,
    IrysPeerId, NodeConfig, PeerAddress, RethPeerInfo,
};
use tracing::{debug, error, info};

#[test_log::test(tokio::test)]
async fn peer_discovery() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.trusted_peers = vec![];
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().block_migration_depth = 1;
    config.storage.num_writes_before_sync = 1;
    // Use hardcoded key for deterministic consensus hash
    let key_bytes: [u8; 32] = [
        0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
        0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88, 0x99,
    ];
    let signing_key = k256::ecdsa::SigningKey::from_bytes((&key_bytes).into()).unwrap();
    let signer = IrysSigner {
        signer: signing_key,
        chain_id: config.consensus_config().chain_id,
        chunk_size: config.consensus_config().chunk_size,
    };
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone());
    let node = node.start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    node.node_ctx.start_mining().unwrap();

    let app = node.start_public_api().await;
    let gossip = node.start_mock_gossip_server().await;

    // Invoke the peer list endpoint
    let req = TestRequest::get().uri("/v1/peer-list").to_request();
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
    let mut version_request = HandshakeRequestV2 {
        chain_id: miner_signer_1.chain_id,
        address: PeerAddress {
            gossip: "127.0.0.1:8080".parse().expect("valid socket address"),
            api: "127.0.0.1:8081".parse().expect("valid socket address"),
            execution: RethPeerInfo {
                peering_tcp_addr: "127.0.0.1:8082".parse().unwrap(),
                peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            },
        },
        peer_id: IrysPeerId::random(),
        mining_address: miner_signer_1.address(),
        user_agent: Some(build_user_agent("miner1", "0.1.0")),
        ..Default::default()
    };
    miner_signer_1
        .sign_p2p_handshake_v2(&mut version_request)
        .expect("sign p2p handshake");

    let req = TestRequest::post()
        .uri(&format!("/gossip/v2{}", GossipRoutes::Handshake))
        .peer_addr("127.0.0.1:12345".parse().unwrap())
        .set_json(&version_request)
        .to_request();
    let resp = call_service(&gossip, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: GossipResponse<HandshakeResponse> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");

    // Assert the response is a successful handshake
    match peer_response {
        GossipResponse::Accepted(_handshake_response) => {
            // Test passes
        }
        GossipResponse::Rejected(reason) => {
            panic!("Expected Accepted response, got Rejected: {:?}", reason);
        }
    }

    // Test that we get rejected if the source IP doesn't match the gossip IP
    let req = TestRequest::post()
        .uri(&format!("/gossip/v2{}", GossipRoutes::Handshake))
        .peer_addr("127.0.0.5:12345".parse().unwrap())
        .set_json(&version_request)
        .to_request();
    let resp = call_service(&gossip, req).await;
    let body = read_body(resp).await;

    // Parse String to JSON
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: GossipResponse<HandshakeResponse> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");

    match peer_response {
        GossipResponse::Accepted(_) => panic!("Expected Rejected response, got Accepted"),
        GossipResponse::Rejected(reason) => {
            assert!(matches!(reason, RejectionReason::InvalidCredentials));
        }
    }

    let miner_signer_2 = IrysSigner::random_signer(&config.consensus_config());

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // spellchecker:off
    let version_json = serde_json::json!({
        "version": "0.1.0",
        "protocol_version": "V2",
        "mining_address": "4AV9BV6viDGKWW88TZ65qT1bZDMy",
        "peer_id": "4JaNfJ1tQ2TCLREq6opq6pWGmCJW",
        "chain_id": 1270,
        "address": {
            "gossip": "127.0.0.2:8080",
            "api": "127.0.0.2:8081",
            "execution": {
                "peering_tcp_addr": "127.0.0.2:8082",
                "peer_id": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
            },
        },
        "timestamp": 0,
        "user_agent": "miner2/0.1.0 (macos/aarch64)",
        "consensus_config_hash": "5dGdi2oSdmpFdBGoUKGfHcWoQD4F6d6p8VhY9t9sZdfe",
        "signature": "EoXNEmq4E4uoEoNVUcJ8A5VtteamtDtLcWfMJevjwLfLNDzebVQtFHcoqwTJqxDo8tVZBcTuHoqdNyb27RMEFK1wH"
    });
    // spellchecker:on

    let req = TestRequest::post()
        .uri(&format!("/gossip/v2{}", GossipRoutes::Handshake))
        .peer_addr("127.0.0.2:12345".parse().unwrap())
        .set_json(&version_json) // Pass the JSON value directly
        .to_request();
    let resp = call_service(&gossip, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: GossipResponse<HandshakeResponse> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    debug!("\nParsed Response:");
    debug!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        GossipResponse::Accepted(accepted) => {
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
        GossipResponse::Rejected(rejected) => {
            panic!("Expected Accepted response, got {:?}", rejected)
        }
    }

    let version_json = serde_json::json!({
        "version": "0.1.0",
        "protocol_version": "V2",
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
        "peer_id": miner_signer_2.address(),
        "user_agent": build_user_agent("miner2", "0.1.0"),
        "timestamp": timestamp,
        "consensus_config_hash": config.consensus_config().keccak256_hash(),
        // Signature from another signer (0xaabb...), should fail verification
        "signature": "BGhPXuxCGZAomzoNKMZAr9ZrNEfcTWJgx5ag3N2fCDTVWgjTdc5SQhqQt4PHh6YZTYDnCURVGbuQL7vhEMjt4dhUo"
    });

    let req = TestRequest::post()
        .uri(&format!("/gossip/v2{}", GossipRoutes::Handshake))
        .peer_addr("127.0.0.2:12345".parse().unwrap())
        .set_json(&version_json) // Pass the JSON value directly
        .to_request();
    let resp = call_service(&gossip, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: GossipResponse<HandshakeResponse> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    debug!("\nParsed Response:");
    debug!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        GossipResponse::Accepted(_) => panic!("Expected Rejected response, got Accepted"),
        GossipResponse::Rejected(reason) => {
            assert!(matches!(reason, RejectionReason::InvalidCredentials));
        }
    }

    let miner_signer_3 = IrysSigner::random_signer(&config.consensus_config());
    let mut version_request = HandshakeRequestV2 {
        chain_id: miner_signer_3.chain_id,
        address: PeerAddress {
            gossip: "127.0.0.3:8080".parse().expect("valid socket address"),
            api: "127.0.0.3:8081".parse().expect("valid socket address"),
            execution: RethPeerInfo {
                peering_tcp_addr: "127.0.0.3:8082".parse().unwrap(),
                peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap()
            },
        },
        peer_id: IrysPeerId::random(),
        mining_address: miner_signer_3.address(),
        user_agent: Some(build_user_agent("miner3", "0.1.0")),
        ..Default::default()
    };
    miner_signer_3
        .sign_p2p_handshake_v2(&mut version_request)
        .expect("sign p2p handshake");

    let req = TestRequest::post()
        .uri(&format!("/gossip/v2{}", GossipRoutes::Handshake))
        .peer_addr("127.0.0.3:12345".parse().unwrap())
        .set_json(version_request)
        .to_request();
    let resp = call_service(&gossip, req).await;
    let body = read_body(resp).await;
    let body_str = String::from_utf8(body.to_vec()).expect("Response body is not valid UTF-8");
    let peer_response: GossipResponse<HandshakeResponse> =
        serde_json::from_str(&body_str).expect("Failed to parse JSON");
    println!("\nParsed Response:");
    println!("{}", serde_json::to_string_pretty(&peer_response).unwrap());

    // Verify the version response body contains the previously discovered peers
    match peer_response {
        GossipResponse::Accepted(accepted) => {
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
        GossipResponse::Rejected(_) => panic!("Expected Accepted response, got Rejected"),
    }

    // Verify the peer_list shows all the peers
    let req = TestRequest::get()
        .uri("/v1/peer-list")
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
        [PeerAddress {
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
            }]
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

#[test_log::test(tokio::test)]
async fn heavy3_should_reinitialize_handshakes() -> eyre::Result<()> {
    // TODO: this test should:
    //  1. Peer 1 launched. Peer 1 doesn't have trusted peers.
    //  2. Peer 2 launched. Peer 2 has Peer 1 as trusted peer.
    //  3. Peer 2 should handshake with Peer 1.
    //  4. Peer 1 should add Peer 2 to its peer list as a temp peer
    //  5. Peer 2 should add Peer 1 to its peer list as a trusted peer
    //  6. Peer 2 goes offline or does something like that so Peer 1 removes it from the temp
    //     peer list.
    //  7. Peer 2 comes back online and handshakes with Peer 1 again.
    //  8. Peer 1 should add Peer 2 back to its peer list as a temp peer
    //  9. Stop Peer 1 - this will wipe out temp peers
    //  10. Restart Peer 1 - it should have no peers anymore
    //  11. If Peer 2 makes a request to the gossip endpoint of Peer 1, Peer 1 should return a
    //      response telling Peer 2 that Peer 1 doesn't know Peer 2 and it should re-attempt the
    //      handshake.
    //  12. Peer 2 should handshake with Peer 1 again.
    //  13. Peer 1 should add Peer 2 back to its peer list as a temp peer
    //  14. Both peers should stop. Congratulations, you've just tested peer discovery!

    let max_seconds = 20;

    let mut testing_config_genesis = NodeConfig::testing();
    testing_config_genesis
        .consensus
        .get_mut()
        .mempool
        .tx_anchor_expiry_depth = 20;

    testing_config_genesis
        .consensus
        .get_mut()
        .block_migration_depth = 4;

    info!("Starting GENESIS node");
    // Genesis doesn't have any trusted peers
    let ctx_genesis_node = IrysNodeTest::new_genesis(testing_config_genesis.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;
    info!("GENESIS node started and packing");
    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        0
    );

    // Peer 1 has genesis as trusted peer
    let ctx_peer1_node = ctx_genesis_node.testing_peer();
    info!("Starting PEER1 node");
    let ctx_peer1_node = IrysNodeTest::new(ctx_peer1_node.clone())
        .start_with_name("PEER1")
        .await;
    info!("PEER1 node started");

    // Check that we've added genesis as a trusted peer
    assert_eq!(
        ctx_peer1_node.node_ctx.peer_list.persistable_peers().len(),
        1
    );
    assert_eq!(ctx_peer1_node.node_ctx.peer_list.temporary_peers().len(), 0);

    // Wait for peer1 to handshake with genesis
    let mut elapsed = 0;
    while ctx_genesis_node
        .node_ctx
        .peer_list
        .temporary_peers()
        .is_empty()
        && elapsed < max_seconds
    {
        info!("Waiting for PEER1 to handshake with GENESIS... {}", elapsed);
        elapsed += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    // Genesis now should have peer1 as a temp peer
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        1
    );

    // Stop peer1 node
    let stopped_peer_1 = ctx_peer1_node.stop().await;
    info!("PEER1 stopped");

    // Decreasing peer1 score just to speed up the pruning process
    ctx_genesis_node.node_ctx.peer_list.decrease_peer_score(
        &stopped_peer_1.cfg.miner_address(),
        ScoreDecreaseReason::Offline("Test pruning".to_string()),
    );

    // Wait for genesis to remove peer1 from its temp peer list
    elapsed = 0;
    while !ctx_genesis_node
        .node_ctx
        .peer_list
        .temporary_peers()
        .is_empty()
        && elapsed < max_seconds
    {
        info!(
            "Waiting for GENESIS to remove PEER1 from temp peer list... {}",
            elapsed
        );
        elapsed += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Check that the genesis has pruned peer1 from its temp peer list
    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        0
    );
    assert!(!ctx_genesis_node.node_ctx.sync_state.is_syncing());
    assert!(ctx_genesis_node
        .node_ctx
        .sync_state
        .is_gossip_broadcast_enabled());
    assert!(ctx_genesis_node
        .node_ctx
        .sync_state
        .is_gossip_reception_enabled());
    info!(
        "GENESIS sync state: {:?}",
        ctx_genesis_node.node_ctx.sync_state
    );

    info!("Restarting PEER1 node");
    let ctx_peer1_node = stopped_peer_1.start_with_name("PEER1").await;
    info!("PEER1 restarted");

    // Wait for peer1 to handshake with genesis again
    elapsed = 0;
    while ctx_genesis_node
        .node_ctx
        .peer_list
        .temporary_peers()
        .is_empty()
        && elapsed < max_seconds
    {
        info!(
            "Waiting for PEER1 to handshake with GENESIS again... {}",
            elapsed
        );
        elapsed += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    // Genesis now should have peer1 as a temp peer again
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        1
    );

    let stopped_genesis = ctx_genesis_node.stop().await;
    info!("GENESIS stopped");

    // Restart genesis
    let ctx_genesis_node = stopped_genesis
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;
    info!("GENESIS restarted");

    // Check that genesis has no peers
    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        0
    );

    // We shouldn't be able to get a block
    let res = ctx_peer1_node
        .node_ctx
        .service_senders
        .peer_network
        .request_block_to_be_gossiped_from_network(BlockHash::repeat_byte(1), false, 1)
        .await;
    match res {
        Ok(()) => panic!("Expected error when requesting block from GENESIS"),
        Err(err) => {
            let message = format!("{err:?}");
            assert!(message.contains("Peer requires a handshake"));
        }
    }

    // Wait for peer1 to handshake with genesis when it gets a failed block request
    elapsed = 0;
    while ctx_genesis_node
        .node_ctx
        .peer_list
        .temporary_peers()
        .is_empty()
        && elapsed < max_seconds
    {
        info!(
            "Waiting for PEER1 to handshake with GENESIS again... {}",
            elapsed
        );
        elapsed += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        ctx_genesis_node
            .node_ctx
            .peer_list
            .persistable_peers()
            .len(),
        0
    );
    // Genesis now should have peer1 as a temp peer again
    assert_eq!(
        ctx_genesis_node.node_ctx.peer_list.temporary_peers().len(),
        1
    );

    tokio::join!(ctx_genesis_node.stop(), ctx_peer1_node.stop());
    Ok(())
}
