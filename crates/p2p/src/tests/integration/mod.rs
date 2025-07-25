use super::util::{create_test_chunks, generate_test_tx, GossipServiceTestFixture};
use core::time::Duration;
use irys_actors::mempool_service::MempoolFacade as _;
use irys_types::irys::IrysSigner;
use irys_types::{
    BlockHash, DataTransactionLedger, GossipBroadcastMessage, H256List, IrysBlockHeader, PeerScore,
};
use reth::builder::Block as _;
use reth::primitives::{Block, BlockBody, Header};
use std::sync::Arc;
use tracing::debug;

#[actix_web::test]
async fn heavy_should_broadcast_message_to_an_established_connection() -> eyre::Result<()> {
    let mut gossip_service_test_fixture_1 = GossipServiceTestFixture::new().await;
    let mut gossip_service_test_fixture_2 = GossipServiceTestFixture::new().await;

    gossip_service_test_fixture_1
        .add_peer(&gossip_service_test_fixture_2)
        .await;
    gossip_service_test_fixture_2
        .add_peer(&gossip_service_test_fixture_1)
        .await;

    let (service1_handle, gossip_service1_message_bus) =
        gossip_service_test_fixture_1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) =
        gossip_service_test_fixture_2.run_service().await;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;
    let data = GossipBroadcastMessage::from(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus
        .send(data)
        .expect("Failed to send transaction through message bus");

    // Waiting a little for service 2 to receive the tx over gossip
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Service 2 receives the message from Service 1
    {
        let service2_mempool_txs = gossip_service_test_fixture_2
            .mempool_txs
            .read()
            .expect("Failed to read service 2 mempool transactions");
        eyre::ensure!(
            service2_mempool_txs.len() == 1,
            "Expected 1 transaction in service 2 mempool, but found {}",
            service2_mempool_txs.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_broadcast_message_to_multiple_peers() -> eyre::Result<()> {
    let mut fixtures = vec![
        GossipServiceTestFixture::new().await,
        GossipServiceTestFixture::new().await,
        GossipServiceTestFixture::new().await,
        GossipServiceTestFixture::new().await,
    ];

    // Connect all peers to each other
    for i in 0..fixtures.len() {
        for j in 0..fixtures.len() {
            if i != j {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "just a test - doesn't need to fight the borrow checker this way"
                )]
                fixtures[i].add_peer(&fixtures[j]).await;
            }
        }
    }

    let mut handles = vec![];
    let mut message_buses = vec![];

    // Start all services
    for fixture in &mut fixtures {
        let (handle, bus) = fixture.run_service().await;
        handles.push(handle);
        message_buses.push(bus);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send data from the first peer
    fixtures
        .first()
        .expect("to have a fixture")
        .mempool_stub
        .handle_data_transaction_ingress(generate_test_tx().header)
        .await
        .expect("To handle tx");

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Verify all peers received it
    for fixture in &fixtures {
        {
            let mempool_txs = fixture
                .mempool_txs
                .read()
                .expect("Failed to read peer mempool transactions");
            eyre::ensure!(
                mempool_txs.len() == 1,
                "Expected 1 transaction in peer mempool, but found {}",
                mempool_txs.len()
            );
        }
    }

    for handle in handles {
        handle.stop().await?;
    }

    Ok(())
}

#[actix_web::test]
async fn heavy_should_not_resend_recently_seen_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    fixture1.add_peer(&fixture2).await;
    fixture2.add_peer(&fixture1).await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipBroadcastMessage::from(generate_test_tx().header);

    // Send same data multiple times
    for _ in 0_i32..3_i32 {
        gossip_service1_message_bus
            .send(data.clone())
            .expect("Failed to send duplicate transaction");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Should only receive it once
    {
        let service2_mempool_txs = fixture2
            .mempool_txs
            .read()
            .expect("Failed to read service 2 mempool transactions for deduplication check");
        eyre::ensure!(
            service2_mempool_txs.len() == 1,
            "Expected 1 transaction in service 2 mempool (deduplication check), but found {}",
            service2_mempool_txs.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_broadcast_chunk_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    fixture1.add_peer(&fixture2).await;
    fixture2.add_peer(&fixture1).await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create and send chunk data
    let chunks = create_test_chunks(&generate_test_tx());
    #[expect(clippy::indexing_slicing, reason = "just a test")]
    let data = GossipBroadcastMessage::from(chunks[0].clone());

    gossip_service1_message_bus
        .send(data)
        .expect("Failed to send chunk data");

    tokio::time::sleep(Duration::from_millis(3000)).await;

    {
        let service2_chunks = fixture2
            .mempool_chunks
            .read()
            .expect("Failed to read service 2 mempool chunks");
        eyre::ensure!(
            service2_chunks.len() == 1,
            "Expected 1 chunk in service 2 mempool, but found {}",
            service2_chunks.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_not_broadcast_to_low_reputation_peers() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    // Add peer2 with low reputation
    fixture1
        .add_peer_with_reputation(&fixture2, PeerScore::new(0))
        .await;
    fixture2.add_peer(&fixture1).await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipBroadcastMessage::from(generate_test_tx().header);
    gossip_service1_message_bus
        .send(data)
        .expect("Failed to send transaction to low reputation peer");

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Should not receive data due to low reputation
    {
        let service2_mempool_txs = fixture2
            .mempool_txs
            .read()
            .expect("Failed to read service 2 mempool transactions for reputation check");
        eyre::ensure!(
            service2_mempool_txs.is_empty(),
            "Expected 0 transactions in low reputation peer mempool, but found {}",
            service2_mempool_txs.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_handle_offline_peer_gracefully() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let fixture2 = GossipServiceTestFixture::new().await;

    // Add peer2 but don't start its service
    fixture1.add_peer(&fixture2).await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipBroadcastMessage::from(generate_test_tx().header);

    // Should not panic when peer is offline
    gossip_service1_message_bus
        .send(data)
        .expect("Failed to send transaction to offline peer");

    tokio::time::sleep(Duration::from_millis(3000)).await;

    service1_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_fetch_missing_transactions_for_block() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    fixture1.add_peer(&fixture2).await;
    fixture2.add_peer(&fixture1).await;

    // Create a test block with transactions
    let mut block = IrysBlockHeader {
        block_hash: BlockHash::random(),
        ..IrysBlockHeader::new_mock_header()
    };
    let mut ledger = DataTransactionLedger::default();
    let tx1 = generate_test_tx().header;
    let tx2 = generate_test_tx().header;
    ledger.tx_ids = H256List(vec![tx1.id, tx2.id]);
    debug!("Added transactions to ledger: {:?}", ledger.tx_ids);
    block.data_ledgers.push(ledger);
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block)
        .expect("to sign block header");

    // Set up the mock API client to return the transactions
    fixture2.api_client_stub.txs.insert(tx1.id, tx1.clone());
    fixture2.api_client_stub.txs.insert(tx2.id, tx2.clone());

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send(GossipBroadcastMessage::from(Arc::new(block)))
        .expect("Failed to send block to service 2");

    // Wait for service 2 to process the block and fetch transactions
    tokio::time::sleep(Duration::from_millis(3000)).await;

    {
        // Check that service 2 received and processed the transactions
        let service2_mempool_txs = fixture2.mempool_txs.read().expect("to read transactions");
        eyre::ensure!(
            service2_mempool_txs.len() == 2,
            "Expected 2 transactions in service 2 mempool after block processing, but found {}",
            service2_mempool_txs.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_reject_block_with_missing_transactions() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    fixture1.add_peer(&fixture2).await;
    fixture2.add_peer(&fixture1).await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Create a test block with transactions
    let mut block = IrysBlockHeader::new_mock_header();
    let mut ledger = DataTransactionLedger::default();
    let tx1 = generate_test_tx().header;
    let tx2 = generate_test_tx().header;
    ledger.tx_ids = H256List(vec![tx1.id, tx2.id]);
    block.data_ledgers.push(ledger);
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block)
        .expect("to sign block header");

    // Set up the mock API client to return only one transaction
    fixture2.api_client_stub.txs.insert(tx1.id, tx1.clone());
    // Don't add tx2 to expected transactions, so it will be missing

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send(GossipBroadcastMessage::from(Arc::new(block)))
        .expect("Failed to send block to service 1");

    // Wait for service 2 to process the block and attempt to fetch transactions
    tokio::time::sleep(Duration::from_millis(3000)).await;

    {
        // Check that service 2 rejected the block due to missing transactions
        let service2_mempool_txs = fixture2.mempool_txs.read().expect("to get mempool txs");
        eyre::ensure!(
            service2_mempool_txs.is_empty(),
            "Expected {} transaction(s), but found {}",
            0,
            service2_mempool_txs.len()
        );
    };

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn heavy_should_gossip_execution_payloads() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new().await;
    let mut fixture2 = GossipServiceTestFixture::new().await;

    fixture1.add_peer(&fixture2).await;
    fixture2.add_peer(&fixture1).await;

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

    // Create a test block with transactions
    let mut block = IrysBlockHeader {
        block_hash: BlockHash::random(),
        evm_block_hash: sealed_block.hash(),
        ..IrysBlockHeader::new_mock_header()
    };
    let signer = IrysSigner::random_signer(&fixture1.config.consensus);
    signer
        .sign_block_header(&mut block)
        .expect("to sign block header");

    let block_execution_data = <<irys_reth_node_bridge::irys_reth::IrysEthereumNode as reth::api::NodeTypes>::Payload as reth::api::PayloadTypes>::block_to_payload(sealed_block.clone());
    fixture1
        .execution_payload_provider
        .add_payload_to_cache(sealed_block.clone())
        .await;

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send(GossipBroadcastMessage::from(Arc::new(block.clone())))
        .expect("Failed to send block to service 2");

    // Wait for service 2 to process the block and receive the execution payload with a timeout of 10 seconds
    fixture2
        .execution_payload_provider
        .test_observe_sealed_block_arrival(block.evm_block_hash, Duration::from_secs(10))
        .await;

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
        .get_locally_stored_sealed_block(&block.evm_block_hash)
        .await
        .expect("to get execution payload stored on peer 1 from peer 2");
    assert_eq!(local_sealed_block, evm_block.seal_slow());

    let payload = fixture2
        .execution_payload_provider
        .wait_for_payload(&block.evm_block_hash)
        .await
        .expect("to wait for execution payload");
    // We compare the payloads because the execution data doesn't implement `PartialEq` directly
    assert_eq!(payload.payload, block_execution_data.payload);

    Ok(())
}
