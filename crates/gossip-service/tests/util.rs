use actix::{Actor, Addr, Context, Handler};
use gossip_service::service::ServiceHandleWithShutdownSignal;
use gossip_service::{GossipData, GossipResult, GossipService, PeerListProvider};
use irys_actors::mempool_service::{
    ChunkIngressError, ChunkIngressMessage, TxIngressError, TxIngressMessage,
};
use irys_primitives::Address;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_types::irys::IrysSigner;
use irys_types::{Config, DatabaseProvider, IrysTransaction, PeerListItem, PeerScore};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct MempoolStub {
    pub txs: Arc<RwLock<Vec<TxIngressMessage>>>,
    pub chunks: Arc<RwLock<Vec<ChunkIngressMessage>>>,
    pub internal_message_bus: mpsc::Sender<GossipData>,
}

impl MempoolStub {
    pub fn new(internal_message_bus: mpsc::Sender<GossipData>) -> Self {
        Self {
            txs: Default::default(),
            chunks: Default::default(),
            internal_message_bus,
        }
    }
}

impl Actor for MempoolStub {
    type Context = Context<Self>;
}

impl Handler<TxIngressMessage> for MempoolStub {
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, msg: TxIngressMessage, _: &mut Self::Context) -> Self::Result {
        let tx = msg.0.clone();
        self.txs.write().unwrap().push(msg);

        let message_bus = self.internal_message_bus.clone();
        tokio::runtime::Handle::current().spawn(async move {
            message_bus.send(GossipData::Transaction(tx)).await.unwrap();
        });

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolStub {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, msg: ChunkIngressMessage, _: &mut Self::Context) -> Self::Result {
        let chunk = msg.0.clone();

        self.chunks.write().unwrap().push(msg);

        let message_bus = self.internal_message_bus.clone();
        tokio::runtime::Handle::current().spawn(async move {
            message_bus.send(GossipData::Chunk(chunk)).await.unwrap();
        });

        Ok(())
    }
}

#[derive(Debug)]
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

        let (rx, _tx) = mpsc::channel(100);

        let mempool_stub = MempoolStub::new(rx);
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

    pub async fn run_service(
        &mut self,
    ) -> (
        ServiceHandleWithShutdownSignal<GossipResult<()>>,
        mpsc::Sender<GossipData>,
    ) {
        let (gossip_service, internal_message_bus) = GossipService::new(
            "127.0.0.1",
            self.port,
            Duration::from_millis(10000),
            self.db.clone(),
        );

        let mempool_stub = MempoolStub::new(internal_message_bus.clone());
        let mempool_txs = mempool_stub.txs.clone();
        let mempool_chunks = mempool_stub.chunks.clone();

        let mempool_stub_addr = mempool_stub.start();
        self.mempool = mempool_stub_addr.clone();
        self.mempool_txs = mempool_txs;
        self.mempool_chunks = mempool_chunks;

        let service_handle = gossip_service.run(mempool_stub_addr).await.unwrap();

        (service_handle, internal_message_bus)
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

    pub fn add_peer(&self, other: &Self) {
        let peer = other.create_default_peer_entry();

        tracing::debug!("Adding peer {:?} to gossip service {:?}", peer, self.port);

        self.peer_list
            .add_peer(&other.mining_address, &peer)
            .unwrap();
    }
}

fn random_free_port() -> u16 {
    // Bind to 127.0.0.1:0 lets the OS assign a random free port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
    listener.local_addr().unwrap().port()
}

pub fn generate_test_tx() -> IrysTransaction {
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
