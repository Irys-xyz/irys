mod chunk_announce;

use super::util::{
    FakeGossipServer, GossipServiceTestFixture, generate_test_tx, poll_until, wait_until_listening,
};
use crate::types::GossipResponse;
use crate::{GossipClient, SyncChainServiceMessage};
use core::time::Duration;
use irys_actors::MempoolFacade as _;
use irys_types::irys::IrysSigner;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
use irys_types::{DataLedger, H256List, IrysBlockHeader, IrysBlockHeaderV1, SendTraced as _};
use reth::builder::Block as _;
use reth::primitives::Header;
use reth_ethereum_primitives::{Block, BlockBody};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::debug;

async fn wait_for_mempool_len(
    fixture: &GossipServiceTestFixture,
    expected_len: usize,
    timeout: Duration,
) -> eyre::Result<()> {
    poll_until(
        timeout,
        &format!("expected {expected_len} transactions in mempool"),
        || {
            fixture
                .mempool_txs
                .read()
                .expect("to read mempool txs")
                .len()
                == expected_len
        },
    )
    .await
}

#[tokio::test]
async fn heavy_should_broadcast_message_to_an_established_connection() -> eyre::Result<()> {
    let mut gossip_service_test_fixture_1 = GossipServiceTestFixture::new();
    let mut gossip_service_test_fixture_2 = GossipServiceTestFixture::new();

    gossip_service_test_fixture_1.add_peer(&gossip_service_test_fixture_2);
    gossip_service_test_fixture_2.add_peer(&gossip_service_test_fixture_1);

    let (service1_handle, gossip_service1_message_bus) =
        gossip_service_test_fixture_1.run_service();
    let (service2_handle, _gossip_service2_message_bus) =
        gossip_service_test_fixture_2.run_service();

    wait_until_listening(gossip_service_test_fixture_1.gossip_addr()).await;
    wait_until_listening(gossip_service_test_fixture_2.gossip_addr()).await;
    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus
        .send_traced(data)
        .expect("Failed to send transaction through message bus");

    poll_until(
        Duration::from_secs(10),
        "Expected 1 transaction in service 2 mempool",
        || {
            gossip_service_test_fixture_2
                .mempool_txs
                .read()
                .expect("read mempool")
                .len()
                == 1
        },
    )
    .await?;

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn should_broadcast_message_to_multiple_peers() -> eyre::Result<()> {
    let mut fixtures = vec![
        GossipServiceTestFixture::new(),
        GossipServiceTestFixture::new(),
        GossipServiceTestFixture::new(),
        GossipServiceTestFixture::new(),
    ];

    // Connect all peers to each other
    for i in 0..fixtures.len() {
        for j in 0..fixtures.len() {
            if i != j {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "just a test - doesn't need to fight the borrow checker this way"
                )]
                fixtures[i].add_peer(&fixtures[j]);
            }
        }
    }

    let mut handles = vec![];
    let mut message_buses = vec![];

    // Start all services
    for fixture in &mut fixtures {
        let (handle, bus) = fixture.run_service();
        handles.push(handle);
        message_buses.push(bus);
    }

    for fixture in &fixtures {
        wait_until_listening(fixture.gossip_addr()).await;
    }

    // Send data from the first peer
    fixtures
        .first()
        .expect("to have a fixture")
        .mempool_stub
        .handle_data_transaction_ingress_gossip(generate_test_tx().header)
        .await
        .expect("To handle tx");

    poll_until(
        Duration::from_secs(10),
        "Not all peers received 1 transaction within timeout",
        || {
            fixtures
                .iter()
                .all(|f| f.mempool_txs.read().expect("read mempool").len() == 1)
        },
    )
    .await?;

    for handle in handles {
        handle.stop().await?;
    }

    Ok(())
}

#[tokio::test]
async fn should_not_resend_recently_seen_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();

    wait_until_listening(fixture1.gossip_addr()).await;
    wait_until_listening(fixture2.gossip_addr()).await;

    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Send same data multiple times with pacing to test dedup
    for _ in 0_i32..3_i32 {
        gossip_service1_message_bus
            .send_traced(data.clone()) // clone: test loop sends the same data multiple times
            .expect("Failed to send duplicate transaction");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    poll_until(
        Duration::from_secs(10),
        "Expected 1 transaction in service 2 mempool (dedup check)",
        || fixture2.mempool_txs.read().expect("read mempool").len() == 1,
    )
    .await?;

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn heavy3_should_broadcast_chunk_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _) = fixture2.run_service();

    wait_until_listening(fixture1.gossip_addr()).await;
    wait_until_listening(fixture2.gossip_addr()).await;

    // Create and send chunk data
    let chunk = chunk_announce::valid_chunk(chunk_announce::chunk_size(&fixture1), 1);
    let data = GossipBroadcastMessageV2::from(chunk);

    gossip_service1_message_bus
        .send_traced(data)
        .expect("Failed to send chunk data");

    poll_until(
        Duration::from_secs(10),
        "Expected 1 chunk in service 2 mempool",
        || {
            fixture2
                .mempool_chunks
                .read()
                .expect("read mempool chunks")
                .len()
                == 1
        },
    )
    .await?;

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn should_handle_offline_peer_gracefully() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let fixture2 = GossipServiceTestFixture::new();

    // Add peer2 but don't start its service
    fixture1.add_peer(&fixture2);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();

    wait_until_listening(fixture1.gossip_addr()).await;

    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Should not panic when peer is offline — connection refused resolves quickly
    gossip_service1_message_bus
        .send_traced(data)
        .expect("Failed to send transaction to offline peer");

    // Wait for the gossip client to attempt the connection and handle the error.
    // When the connection to the offline peer fails, it is marked offline in the peer list.
    let fixture2_peer_id = fixture2.config.peer_id();
    poll_until(
        Duration::from_secs(10),
        "Expected offline peer to be marked as not online after failed connection attempt",
        || {
            fixture1
                .peer_list
                .get_peer(&fixture2_peer_id)
                .is_some_and(|p| !p.is_online)
        },
    )
    .await?;

    service1_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn should_fetch_missing_transactions_for_block() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    // Create a test block with transactions, connecting to fixture2's genesis
    let genesis = fixture2.block_status_provider.genesis_header();
    let mut block_header = IrysBlockHeader::V1(IrysBlockHeaderV1 {
        height: genesis.height + 1,
        previous_block_hash: genesis.block_hash,
        ..IrysBlockHeaderV1::new_mock_header()
    });
    let tx1 = generate_test_tx().header;
    let tx2 = generate_test_tx().header;
    block_header.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx1.id, tx2.id]);
    debug!(
        "Added transactions to Submit ledger: {:?}",
        block_header.data_ledgers[DataLedger::Submit].tx_ids
    );
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block_header)
        .expect("to sign block header");

    // Set up the mock API client to return the transactions
    fixture1.add_tx_to_mempool(tx1.clone()).await;
    fixture1.add_tx_to_mempool(tx2.clone()).await;
    fixture1.persist_block_header_to_db(&block_header);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();

    wait_until_listening(fixture1.gossip_addr()).await;
    wait_until_listening(fixture2.gossip_addr()).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send_traced(GossipBroadcastMessageV2::from(Arc::new(block_header)))
        .expect("Failed to send block to service 2");

    wait_for_mempool_len(&fixture2, 2, Duration::from_secs(10)).await?;

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn should_reject_block_with_missing_transactions() -> eyre::Result<()> {
    let fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    // Create a test block, connecting to fixture2's genesis, whose Submit ledger lists two txs
    let genesis = fixture2.block_status_provider.genesis_header();
    let mut block_header = IrysBlockHeader::V1(IrysBlockHeaderV1 {
        height: genesis.height + 1,
        previous_block_hash: genesis.block_hash,
        ..IrysBlockHeaderV1::new_mock_header()
    });
    let tx1 = generate_test_tx().header;
    let tx2 = generate_test_tx().header;
    block_header.data_ledgers[DataLedger::Submit].tx_ids = H256List(vec![tx1.id, tx2.id]);
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block_header)
        .expect("to sign block header");
    let block_hash = block_header.block_hash;

    // An honest node cannot serve this body. A malicious peer can: the body carries tx1 only,
    // so `SealedBlock::new` rejects it because the header lists tx2 and the body lacks it.
    let crafted_body = irys_types::BlockBody {
        block_hash,
        data_transactions: vec![tx1],
        ..Default::default()
    };
    let body_requests = Arc::new(AtomicUsize::new(0));
    let fake_peer = FakeGossipServer::new();
    {
        let body_requests = Arc::clone(&body_requests);
        fake_peer.set_on_pull_data_request(move |request| match request {
            GossipDataRequestV2::BlockBody(_) => {
                body_requests.fetch_add(1, Ordering::SeqCst);
                GossipResponse::Accepted(Some(GossipDataV2::BlockBody(Arc::new(
                    crafted_body.clone(),
                ))))
            }
            _ => GossipResponse::Accepted(None),
        });
    }
    let fake_peer_gossip_addr = fake_peer.spawn();

    // fixture2 knows fixture1 (the header's source) at the fake server's address, so every body
    // pull goes to the fake peer.
    let mut fixture1_peer_entry = fixture1.create_default_peer_entry();
    fixture1_peer_entry.address.gossip = fake_peer_gossip_addr;
    fixture2
        .peer_list
        .add_or_update_peer(fixture1_peer_entry, true);

    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();
    wait_until_listening(fixture2.gossip_addr()).await;

    // Push the header to fixture2, signing the request as fixture1.
    let fixture1_gossip_client = GossipClient::new(
        Duration::from_secs(5),
        fixture1.config.node_config.miner_address(),
        fixture1.config.peer_id(),
        tokio::runtime::Handle::current(),
        fixture1.config.node_config.max_peer_response_bytes,
    );
    let fixture2_peer_entry = fixture2.create_default_peer_entry();

    // Rejecting the served body in `SealedBlock::new` costs the peer a BogusData penalty (-5),
    // and the answered request earns +1, so one rejection lowers the score by 4. The other
    // per-request penalties (slow response, network error) are -1, so a drop of at least 4 means
    // the crafted body was served and rejected.
    let fixture1_peer_id = fixture1.config.peer_id();
    let initial_score = fixture2
        .peer_list
        .get_peer(&fixture1_peer_id)
        .expect("fixture1 should be a known peer")
        .reputation_score;
    fixture1_gossip_client.send_data_without_score_update(
        (&fixture2.config.peer_id(), &fixture2_peer_entry),
        Arc::new(GossipDataV2::BlockHeader(Arc::new(block_header))),
    );

    poll_until(
        Duration::from_secs(10),
        "Expected fixture2 to penalise the peer that served a block body with a missing tx",
        || {
            body_requests.load(Ordering::SeqCst) >= 1
                && fixture2
                    .peer_list
                    .get_peer(&fixture1_peer_id)
                    .is_some_and(|p| {
                        initial_score.get().saturating_sub(p.reputation_score.get()) >= 4
                    })
        },
    )
    .await?;

    eyre::ensure!(
        fixture2
            .mempool_txs
            .read()
            .expect("to read mempool")
            .is_empty(),
        "no transaction from the rejected body may reach the mempool"
    );
    eyre::ensure!(
        fixture2
            .discovery_blocks
            .read()
            .expect("to read blocks")
            .is_empty(),
        "the rejected block must not reach block discovery"
    );

    service2_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn should_gossip_execution_payloads() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let evm_block = Block {
        header: Header {
            parent_hash: Default::default(),
            number: 1,
            gas_limit: 10,
            gas_used: 10,
            timestamp: 10,
            extra_data: Default::default(),
            base_fee_per_gas: Some(10),
            ..Default::default()
        },
        body: BlockBody::default(),
    };
    let sealed_block = evm_block.clone().seal_slow();

    // Create a test block connecting to fixture2's genesis
    let genesis = fixture2.block_status_provider.genesis_header();
    let mut block = IrysBlockHeader::V1(IrysBlockHeaderV1 {
        height: genesis.height + 1,
        previous_block_hash: genesis.block_hash,
        evm_block_hash: sealed_block.hash(),
        ..IrysBlockHeaderV1::new_mock_header()
    });
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block)
        .expect("to sign block header");

    let block_execution_data = <<irys_reth_node_bridge::irys_reth::IrysEthereumNode as reth::api::NodeTypes>::Payload as reth::api::PayloadTypes>::block_to_payload(sealed_block.clone());
    fixture1
        .execution_payload_provider
        .add_payload_to_cache(sealed_block.clone())
        .await
        .expect("payload body matches its header");
    fixture1.persist_block_header_to_db(&block);

    let execution_payload_provider2 = fixture2.execution_payload_provider.clone();
    let mut sync_rx = fixture2._sync_rx.take().expect("expect to have a sync rx");
    tokio::spawn(async move {
        loop {
            if let Some(SyncChainServiceMessage::PullPayloadFromTheNetwork {
                evm_block_hash,
                use_trusted_peers_only: _,
                response,
            }) = sync_rx.recv().await
            {
                debug!("Sync request received for block hash: {:?}", evm_block_hash);
                if evm_block_hash == sealed_block.hash() {
                    execution_payload_provider2
                        .add_payload_to_cache(sealed_block.clone())
                        .await
                        .expect("payload body matches its header");
                    response.send(Ok(())).expect("to deliver response");
                    break;
                } else {
                    debug!(
                        "Received sync request for unknown block hash: {:?}",
                        evm_block_hash
                    );
                    break;
                }
            }
        }
    });

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();

    wait_until_listening(fixture1.gossip_addr()).await;
    wait_until_listening(fixture2.gossip_addr()).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send_traced(GossipBroadcastMessageV2::from(Arc::new(block.clone()))) // clone: block used later in assertion
        .expect("Failed to send block to service 2");

    // Wait for service 2 to process the block and receive the execution payload with a timeout of 10 seconds
    fixture2
        .execution_payload_provider
        .test_observe_sealed_block_arrival(block.evm_block_hash, Duration::from_secs(10))
        .await?;

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    let local_block = fixture2
        .execution_payload_provider
        .get_locally_stored_evm_block(&block.evm_block_hash)
        .await
        .expect("to get execution payload stored on peer 1 from peer 2");
    assert_eq!(local_block, evm_block);

    let local_sealed_block = fixture2
        .execution_payload_provider
        .get_sealed_block_from_cache(&block.evm_block_hash)
        .await
        .expect("to get execution payload stored on peer 1 from peer 2");
    assert_eq!(local_sealed_block, evm_block.seal_slow());

    let payload = fixture2
        .execution_payload_provider
        .wait_for_payload(&block.evm_block_hash)
        .await
        .expect("to wait for execution payload");
    // `wait_for_payload` returns `Result` — `.expect()` on `Result` panics
    // with the typed error, preserving the existing test semantics.
    // We compare the payloads because the execution data doesn't implement `PartialEq` directly
    assert_eq!(payload.payload, block_execution_data.payload);

    Ok(())
}
