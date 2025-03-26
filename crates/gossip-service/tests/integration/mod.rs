use crate::util::{create_test_chunks, generate_test_tx, GossipServiceTestFixture};
use gossip_service::GossipData;
use std::time::Duration;
use irys_types::PeerScore;

#[actix_web::test]
async fn should_broadcast_message_to_an_established_connection() -> eyre::Result<()> {
    let mut gossip_service_test_fixture_1 = GossipServiceTestFixture::new();
    let mut gossip_service_test_fixture_2 = GossipServiceTestFixture::new();

    gossip_service_test_fixture_1.add_peer(&gossip_service_test_fixture_2);
    gossip_service_test_fixture_2.add_peer(&gossip_service_test_fixture_1);

    let (service1_handle, gossip_service1_message_bus) =
        gossip_service_test_fixture_1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) =
        gossip_service_test_fixture_2.run_service().await;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipData::Transaction(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus.send(data).await.unwrap();

    // Waiting a little for service 2 to receive the tx over gossip
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Service 2 receives the message from Service 1
    let service2_mempool_txs = gossip_service_test_fixture_2.mempool_txs.read().unwrap();
    assert_eq!(service2_mempool_txs.len(), 1);
    // The tx also must be in the first node's mempool
    let service1_mempool_txs = gossip_service_test_fixture_1.mempool_txs.read().unwrap();
    assert_eq!(service1_mempool_txs.len(), 1);

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
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
                fixtures[i].add_peer(&fixtures[j]);
            }
        }
    }

    let mut handles = vec![];
    let mut message_buses = vec![];
    
    // Start all services
    for fixture in fixtures.iter_mut() {
        let (handle, bus) = fixture.run_service().await;
        handles.push(handle);
        message_buses.push(bus);
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send data from first peer
    let data = GossipData::Transaction(generate_test_tx().header);
    message_buses[0].send(data).await.unwrap();

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Verify all peers received it
    for fixture in fixtures.iter() {
        let mempool_txs = fixture.mempool_txs.read().unwrap();
        assert_eq!(mempool_txs.len(), 1);
    }

    for handle in handles {
        handle.stop().await?;
    }

    Ok(())
}

#[actix_web::test]
async fn should_not_resend_recently_seen_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _gossip_service2_message_bus) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipData::Transaction(generate_test_tx().header);

    // Send same data multiple times
    for _ in 0..3 {
        gossip_service1_message_bus.send(data.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Should only receive it once
    let service2_mempool_txs = fixture2.mempool_txs.read().unwrap();
    assert_eq!(service2_mempool_txs.len(), 1);

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn should_broadcast_chunk_data() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    fixture1.add_peer(&fixture2);
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create and send chunk data
    let chunks = create_test_chunks(&generate_test_tx());
    let data = GossipData::Chunk(chunks[0].clone());
    
    gossip_service1_message_bus.send(data).await.unwrap();

    tokio::time::sleep(Duration::from_millis(3000)).await;

    let service2_chunks = fixture2.mempool_chunks.read().unwrap();
    assert_eq!(service2_chunks.len(), 1);

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn should_not_broadcast_to_low_reputation_peers() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let mut fixture2 = GossipServiceTestFixture::new();

    // Add peer2 with low reputation
    fixture1.add_peer_with_reputation(&fixture2, PeerScore::new(0));
    fixture2.add_peer(&fixture1);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;
    let (service2_handle, _) = fixture2.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipData::Transaction(generate_test_tx().header);
    gossip_service1_message_bus.send(data).await.unwrap();

    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Should not receive data due to low reputation
    let service2_mempool_txs = fixture2.mempool_txs.read().unwrap();
    assert_eq!(service2_mempool_txs.len(), 0);

    service1_handle.stop().await?;
    service2_handle.stop().await?;

    Ok(())
}

#[actix_web::test]
async fn should_handle_offline_peer_gracefully() -> eyre::Result<()> {
    let mut fixture1 = GossipServiceTestFixture::new();
    let fixture2 = GossipServiceTestFixture::new();

    // Add peer2 but don't start its service
    fixture1.add_peer(&fixture2);

    let (service1_handle, gossip_service1_message_bus) = fixture1.run_service().await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let data = GossipData::Transaction(generate_test_tx().header);
    
    // Should not panic when peer is offline
    gossip_service1_message_bus.send(data).await.unwrap();

    tokio::time::sleep(Duration::from_millis(3000)).await;

    service1_handle.stop().await?;

    Ok(())
}
