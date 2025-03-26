use crate::util::{GossipServiceTestFixture, generate_test_tx};
use gossip_service::{GossipData};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

#[actix_web::test]
async fn should_broadcast_message_to_an_established_connection() -> eyre::Result<()> {
    let gossip_service_test_fixture_1 = GossipServiceTestFixture::new();
    let gossip_service_test_fixture_2 = GossipServiceTestFixture::new();

    gossip_service_test_fixture_1.add_peer(&gossip_service_test_fixture_2);
    gossip_service_test_fixture_2.add_peer(&gossip_service_test_fixture_1);

    let (gossip_service1, gossip_service1_message_bus) =
        gossip_service_test_fixture_1.create_gossip_service();
    let (gossip_service2, _gossip_service2_message_bus) =
        gossip_service_test_fixture_2.create_gossip_service();

    let service1_handle = gossip_service1.run().await?;
    let service2_handle = gossip_service2.run().await?;

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    let origin = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345);
    let data = GossipData::Transaction(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus
        .send((origin, data))
        .await
        .unwrap();

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
