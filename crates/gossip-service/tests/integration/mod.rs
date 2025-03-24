use crate::util::MempoolStub;
use actix::{Actor, Addr, Handler};
use gossip_service::{GossipData, GossipService, PeerListProvider};
use irys_actors::mempool_service::{ChunkIngressMessage, TxIngressMessage};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_types::irys::IrysSigner;
use irys_types::Config;
use irys_types::{Address, DatabaseProvider, IrysTransaction, PeerListItem, PeerScore};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct GossipServiceTestFixture {
    pub temp_dir: TempDir,
    pub port: u16,
    pub db: DatabaseProvider,
    pub peer_list: PeerListProvider,
    pub mining_address: Address,
    pub mempool: Addr<MempoolStub>,
    pub mempool_txs: Arc<RwLock<Vec<TxIngressMessage>>>,
    pub mempool_chunks: Arc<RwLock<Vec<ChunkIngressMessage>>>,
}

impl GossipServiceTestFixture {
    pub fn new() -> Self {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let port = random_free_port();
        let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf()).unwrap();
        let db = DatabaseProvider(Arc::new(db_env));
        let peer_list = PeerListProvider::new(db.clone());

        let mempool_stub = MempoolStub::new();
        let mempool_txs = mempool_stub.txs.clone();
        let mempool_chunks = mempool_stub.chunks.clone();

        let mempool_stub_addr = mempool_stub.start();

        Self {
            temp_dir,
            port,
            db,
            peer_list,
            mining_address: Address::random(),
            mempool: mempool_stub_addr,
            mempool_txs,
            mempool_chunks,
        }
    }

    pub fn create_gossip_service(
        &self,
    ) -> (
        GossipService<MempoolStub>,
        mpsc::Sender<(SocketAddr, GossipData)>,
    ) {
        GossipService::new(
            "127.0.0.1",
            self.port,
            Duration::from_millis(10000),
            self.db.clone(),
            self.mempool.clone(),
        )
    }

    pub fn create_default_peer_entry(&self) -> PeerListItem {
        PeerListItem {
            reputation_score: PeerScore::new(50),
            response_time: 0,
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.port),
            last_seen: 0,
            is_online: true,
        }
    }
}

fn random_free_port() -> u16 {
    // Bind to 127.0.0.1:0 lets the OS assign a random free port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
    listener.local_addr().unwrap().port()
}

fn generate_test_tx() -> IrysTransaction {
    let testnet_config = Config::testnet();
    let account1 = IrysSigner::random_signer(&testnet_config);
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    // post a tx, mine a block
    let tx = account1
        .create_transaction(data_bytes.clone(), None)
        .unwrap();
    account1.sign_transaction(tx).unwrap()
}

#[actix_web::test]
async fn should_broadcast_message_to_an_established_connection() -> eyre::Result<()> {
    let gossip_service_test_fixture_1 = GossipServiceTestFixture::new();
    let gossip_service_test_fixture_2 = GossipServiceTestFixture::new();

    let peer_1 = gossip_service_test_fixture_1.create_default_peer_entry();
    let peer_2 = gossip_service_test_fixture_2.create_default_peer_entry();

    gossip_service_test_fixture_1
        .peer_list
        .add_peer(&gossip_service_test_fixture_2.mining_address, &peer_2)
        .unwrap();

    gossip_service_test_fixture_2
        .peer_list
        .add_peer(&gossip_service_test_fixture_1.mining_address, &peer_1)
        .unwrap();

    let (gossip_service1, gossip_service1_message_bus) =
        gossip_service_test_fixture_1.create_gossip_service();
    let (gossip_service2, gossip_service2_message_bus) =
        gossip_service_test_fixture_2.create_gossip_service();

    // Waiting a little for the service to initialize
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let origin = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345);
    let data = GossipData::Transaction(generate_test_tx().header);

    // Service 1 receives a message through the message bus from a system's component
    gossip_service1_message_bus
        .send((origin, data))
        .await
        .unwrap();

    // Waiting a little for service 2 to receive the tx over gossip
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Service 2 receives the message from Service 1
    let service2_mempool_txs = gossip_service_test_fixture_2.mempool_txs.read().unwrap();
    assert_eq!(service2_mempool_txs.len(), 1);

    Ok(())
}
