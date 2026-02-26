use super::util::{create_test_chunks, generate_test_tx, GossipServiceTestFixture};
use crate::SyncChainServiceMessage;
use core::time::Duration;
use irys_actors::MempoolFacade as _;
use irys_types::irys::IrysSigner;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    DataLedger, DataTransactionLedger, H256List, IrysBlockHeader, IrysBlockHeaderV1,
    SendTraced as _,
};
use reth::builder::Block as _;
use reth::primitives::{Block, BlockBody, Header};
use std::sync::Arc;
use tracing::debug;

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

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;
    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus
        .send_traced(data)
        .expect("Failed to send transaction through message bus");

    // Service 2 receives it over gossip
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

#[tokio::test]
async fn heavy_should_broadcast_message_to_multiple_peers() -> eyre::Result<()> {
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

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send data from the first peer
    fixtures
        .first()
        .expect("to have a fixture")
        .mempool_stub
        .handle_data_transaction_ingress_gossip(generate_test_tx().header)
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

#[tokio::test]
async fn heavy3_should_not_resend_recently_seen_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Send same data multiple times
    for _ in 0_i32..3_i32 {
        gossip_service1_message_bus
            .send_traced(data.clone()) // clone: test loop sends the same data multiple times
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

#[tokio::test]
async fn heavy_should_broadcast_chunk_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _) = fixture2.run_service();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create and send chunk data
    let chunks = create_test_chunks(&generate_test_tx());
    #[expect(clippy::indexing_slicing, reason = "just a test")]
    let data = GossipBroadcastMessageV2::from(chunks[0].clone());

    gossip_service1_message_bus
        .send_traced(data)
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

#[tokio::test]
async fn heavy_should_handle_offline_peer_gracefully() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let fixture2 = GossipServiceTestFixture::new();

    // Add peer2 but don't start its service
    fixture1.add_peer(&fixture2);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipBroadcastMessageV2::from(generate_test_tx().header);

    // Should not panic when peer is offline
    gossip_service1_message_bus
        .send_traced(data)
        .expect("Failed to send transaction to offline peer");

    tokio::time::sleep(Duration::from_millis(3000)).await;

    service1_handle.stop().await?;

    Ok(())
}

#[tokio::test]
async fn heavy_should_fetch_missing_transactions_for_block() -> eyre::Result<()> {
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

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send_traced(GossipBroadcastMessageV2::from(Arc::new(block_header)))
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

#[tokio::test]
async fn heavy_should_reject_block_with_missing_transactions() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service();
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service();

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
    fixture1.add_tx_to_mempool(tx1.clone()).await;
    // Don't add tx2 to expected transactions, so it will be missing

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send_traced(GossipBroadcastMessageV2::from(Arc::new(block)))
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

#[tokio::test]
async fn heavy_should_gossip_execution_payloads() -> eyre::Result<()> {
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
        .await;
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
                        .await;
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

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send block from service 1 to service 2
    gossip_service1_message_bus
        .send_traced(GossipBroadcastMessageV2::from(Arc::new(block.clone()))) // clone: block used later in assertion
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
        .get_sealed_block_from_cache(&block.evm_block_hash)
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
