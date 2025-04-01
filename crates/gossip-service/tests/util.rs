use actix::{Actor, Addr, Context, Handler};
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use eyre::Result;
use irys_actors::mempool_service::{
    ChunkIngressError, ChunkIngressMessage, TxExistenceQuery, TxIngressError, TxIngressMessage,
};
use irys_api_client::ApiClient;
use irys_gossip_service::service::ServiceHandleWithShutdownSignal;
use irys_gossip_service::{GossipResult, GossipService, PeerListProvider};
use irys_primitives::Address;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_testing_utils::utils::tempfile::TempDir;
use irys_types::irys::IrysSigner;
use irys_types::{
    Base64, Config, DatabaseProvider, GossipData, IrysTransaction, IrysTransactionHeader,
    PeerListItem, PeerScore, TxChunkOffset, UnpackedChunk, H256,
};
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tracing::debug;

#[derive(Debug)]
pub struct MempoolStub {
    pub txs: Arc<RwLock<Vec<TxIngressMessage>>>,
    pub chunks: Arc<RwLock<Vec<ChunkIngressMessage>>>,
    pub internal_message_bus: mpsc::Sender<GossipData>,
}

impl MempoolStub {
    #[must_use]
    pub fn new(internal_message_bus: mpsc::Sender<GossipData>) -> Self {
        Self {
            txs: Arc::default(),
            chunks: Arc::default(),
            internal_message_bus,
        }
    }
}

impl Actor for MempoolStub {
    type Context = Context<Self>;
}

impl Handler<TxIngressMessage> for MempoolStub {
    type Result = Result<(), TxIngressError>;

    /// # Panics
    /// Can panic
    fn handle(&mut self, msg: TxIngressMessage, _: &mut Self::Context) -> Self::Result {
        let tx = msg.0.clone();

        let already_exists = self
            .txs
            .read()
            .expect("to unlock mempool txs")
            .iter()
            .any(|message| message.0 == msg.0);

        if already_exists {
            return Err(TxIngressError::Skipped);
        }

        self.txs
            .write()
            .expect("to unlock txs in the mempool stub")
            .push(msg);
        // Pretend that we've validated the tx and we're ready to gossip it
        let message_bus = self.internal_message_bus.clone();
        tokio::runtime::Handle::current().spawn(async move {
            message_bus
                .send(GossipData::Transaction(tx))
                .await
                .expect("to send transaction");
        });

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolStub {
    type Result = Result<(), ChunkIngressError>;

    /// # Panics
    /// Can panic
    fn handle(&mut self, msg: ChunkIngressMessage, _: &mut Self::Context) -> Self::Result {
        let chunk = msg.0.clone();

        self.chunks
            .write()
            .expect("to unlock mempool chunks")
            .push(msg);

        // Pretend that we've validated the chunk and we're ready to gossip it
        let message_bus = self.internal_message_bus.clone();
        tokio::runtime::Handle::current().spawn(async move {
            message_bus
                .send(GossipData::Chunk(chunk))
                .await
                .expect("to send chunk");
        });

        Ok(())
    }
}

impl Handler<TxExistenceQuery> for MempoolStub {
    type Result = Result<bool, TxIngressError>;

    /// # Panics
    /// Can panic
    fn handle(&mut self, msg: TxExistenceQuery, _: &mut Self::Context) -> Self::Result {
        let tx_id = msg.0;
        let exists = self
            .txs
            .read()
            .expect("to read txs")
            .iter()
            .any(|message| message.0.id == tx_id);
        Ok(exists)
    }
}

#[derive(Debug, Clone)]
pub struct StubApiClient {
    pub txs: HashMap<H256, IrysTransactionHeader>,
}

#[async_trait::async_trait]
impl ApiClient for StubApiClient {
    async fn get_transaction(
        &self,
        _peer: SocketAddr,
        tx_id: H256,
    ) -> Result<Option<IrysTransactionHeader>> {
        println!("Fetching transaction {:?} from stub API client", tx_id);
        println!("{:?}", self.txs.get(&tx_id));
        Ok(self.txs.get(&tx_id).cloned())
    }

    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<Option<IrysTransactionHeader>>> {
        debug!("Fetching {} transactions from peer {}", tx_ids.len(), peer);
        let mut results = Vec::with_capacity(tx_ids.len());

        for &tx_id in tx_ids {
            let result = self.get_transaction(peer, tx_id).await?;
            results.push(result);
        }

        Ok(results)
    }
}

impl Default for StubApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl StubApiClient {
    #[must_use]
    pub fn new() -> Self {
        Self {
            txs: HashMap::new(),
        }
    }

    pub fn add_transaction(&mut self, tx_id: H256, tx_header: IrysTransactionHeader) {
        self.txs.insert(tx_id, tx_header);
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
    pub api_client: StubApiClient,
}

impl Default for GossipServiceTestFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl GossipServiceTestFixture {
    /// # Panics
    /// Can panic
    #[must_use]
    pub fn new() -> Self {
        let temp_dir = setup_tracing_and_temp_dir(Some("gossip_test_fixture"), false);
        let port = random_free_port();
        let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir");
        let db = DatabaseProvider(Arc::new(db_env));
        let peer_list = PeerListProvider::new(db.clone());

        let (rx, _tx) = mpsc::channel(100);

        let mempool_stub = MempoolStub::new(rx);
        let mempool_txs = Arc::clone(&mempool_stub.txs);
        let mempool_chunks = Arc::clone(&mempool_stub.chunks);

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
            api_client: StubApiClient::new(),
        }
    }

    /// # Panics
    /// Can panic
    pub fn run_service(
        &mut self,
    ) -> (
        ServiceHandleWithShutdownSignal<GossipResult<()>>,
        mpsc::Sender<GossipData>,
    ) {
        let (gossip_service, internal_message_bus) =
            GossipService::new("127.0.0.1", self.port, self.db.clone());

        let mempool_stub = MempoolStub::new(internal_message_bus.clone());
        self.mempool_txs = Arc::clone(&mempool_stub.txs);
        self.mempool_chunks = Arc::clone(&mempool_stub.chunks);

        let mempool_stub_addr = mempool_stub.start();
        self.mempool = mempool_stub_addr.clone();

        let api_client = self.api_client.clone();

        let service_handle = gossip_service
            .run(mempool_stub_addr, api_client)
            .expect("failed to run gossip service");

        (service_handle, internal_message_bus)
    }

    #[must_use]
    pub fn create_default_peer_entry(&self) -> PeerListItem {
        PeerListItem {
            reputation_score: PeerScore::new(50),
            response_time: 0,
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.port),
            last_seen: 0,
            is_online: true,
        }
    }

    /// # Panics
    /// Can panic
    pub fn add_peer(&self, other: &Self) {
        let peer = other.create_default_peer_entry();

        tracing::debug!("Adding peer {:?} to gossip service {:?}", peer, self.port);

        self.peer_list
            .add_peer(&other.mining_address, &peer)
            .expect("to add peer");
    }

    /// # Panics
    /// Can panic
    pub fn add_peer_with_reputation(&self, other: &Self, score: PeerScore) {
        let peer = PeerListItem {
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), other.port),
            reputation_score: score,
            is_online: true,
            ..PeerListItem::default()
        };
        self.peer_list
            .add_peer(&other.mining_address, &peer)
            .expect("to add a peer");
    }
}

fn random_free_port() -> u16 {
    // Bind to 127.0.0.1:0 lets the OS assign a random free port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
    listener.local_addr().expect("to get a port").port()
}

/// # Panics
/// Can panic
#[must_use]
pub fn generate_test_tx() -> IrysTransaction {
    let testnet_config = Config::testnet();
    let account1 = IrysSigner::random_signer(&testnet_config);
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    // post a tx, mine a block
    let tx = account1
        .create_transaction(data_bytes, None)
        .expect("Failed to create transaction");
    account1
        .sign_transaction(tx)
        .expect("signing transaction failed")
}

#[must_use]
pub fn create_test_chunks(tx: &IrysTransaction) -> Vec<UnpackedChunk> {
    let mut chunks = Vec::new();
    for _chunk_node in &tx.chunks {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let data_path = Base64(vec![1, 2, 3]);

        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(vec![1, 2, 3]),
            tx_offset: TxChunkOffset::from(0_i32),
        };

        chunks.push(chunk);
    }

    chunks
}
