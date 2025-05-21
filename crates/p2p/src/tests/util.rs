use crate::peer_list::{AddPeer, PeerListServiceWithClient};
use crate::types::GossipDataRequest;
use crate::{P2PService, ServiceHandleWithShutdownSignal};
use actix::{Actor, Addr, Context, Handler};
use actix_web::dev::Server;
use actix_web::{middleware, web, App, HttpResponse, HttpServer};
use async_trait::async_trait;
use base58::ToBase58;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use eyre::{eyre, Result};
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::broadcast_mining_service::BroadcastMiningSeed;
use irys_actors::mempool_service::{ChunkIngressError, MempoolFacade, TxIngressError};
use irys_api_client::ApiClient;
use irys_primitives::Address;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::irys::IrysSigner;
use irys_types::{
    AcceptedResponse, Base64, BlockHash, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
    CommitmentTransaction, Config, DatabaseProvider, GossipData, GossipRequest, IrysBlockHeader,
    IrysTransaction, IrysTransactionHeader, IrysTransactionResponse, NodeConfig, NodeMode,
    PeerAddress, PeerListItem, PeerResponse, PeerScore, RethPeerInfo, TxChunkOffset, UnpackedChunk,
    VersionRequest, H256,
};
use reth_tasks::{TaskExecutor, TaskManager};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub(crate) struct MempoolStub {
    pub txs: Arc<RwLock<Vec<IrysTransactionHeader>>>,
    pub chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub internal_message_bus: mpsc::Sender<GossipData>,
}

impl MempoolStub {
    #[must_use]
    pub(crate) fn new(internal_message_bus: mpsc::Sender<GossipData>) -> Self {
        Self {
            txs: Arc::default(),
            chunks: Arc::default(),
            internal_message_bus,
        }
    }
}

#[async_trait]
impl MempoolFacade for MempoolStub {
    async fn handle_data_transaction(
        &self,
        tx_header: IrysTransactionHeader,
    ) -> std::result::Result<(), TxIngressError> {
        let already_exists = self
            .txs
            .read()
            .expect("to unlock mempool txs")
            .iter()
            .any(|tx| tx == &tx_header);

        if already_exists {
            return Err(TxIngressError::Skipped);
        }

        self.txs
            .write()
            .expect("to unlock txs in the mempool stub")
            .push(tx_header.clone());
        // Pretend that we've validated the tx and we're ready to gossip it
        let message_bus = self.internal_message_bus.clone();
        tokio::runtime::Handle::current().spawn(async move {
            message_bus
                .send(GossipData::Transaction(tx_header))
                .await
                .expect("to send transaction");
        });

        Ok(())
    }

    async fn handle_commitment_transaction(
        &self,
        _tx_header: CommitmentTransaction,
    ) -> std::result::Result<(), TxIngressError> {
        Ok(())
    }

    async fn handle_chunk(
        &self,
        chunk: UnpackedChunk,
    ) -> std::result::Result<(), ChunkIngressError> {
        self.chunks
            .write()
            .expect("to unlock mempool chunks")
            .push(chunk.clone());

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

    async fn is_known_tx(&self, tx_id: H256) -> std::result::Result<bool, TxIngressError> {
        let exists = self
            .txs
            .read()
            .expect("to read txs")
            .iter()
            .any(|message| message.id == tx_id);
        Ok(exists)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockDiscoveryStub {
    pub blocks: Arc<RwLock<Vec<IrysBlockHeader>>>,
    pub internal_message_bus: mpsc::Sender<GossipData>,
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryStub {
    async fn handle_block(&self, block: IrysBlockHeader) -> Result<()> {
        self.blocks
            .write()
            .expect("to unlock blocks")
            .push(block.clone());

        let sender = self.internal_message_bus.clone();

        // Pretend that we've validated the block and we're ready to gossip it
        tokio::runtime::Handle::current().spawn(async move {
            sender
                .send(GossipData::Block(block))
                .await
                .expect("to send block");
        });

        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ApiClientStub {
    pub txs: HashMap<H256, IrysTransactionHeader>,
    pub block_index_handler: Arc<
        RwLock<Box<dyn Fn(BlockIndexQuery) -> Result<Vec<BlockIndexItem>> + Send + Sync + 'static>>,
    >,
    pub block_index_calls: Arc<RwLock<Vec<BlockIndexQuery>>>,
}

impl ApiClientStub {
    pub(crate) fn new() -> Self {
        Self {
            txs: HashMap::new(),
            block_index_handler: Arc::new(RwLock::new(Box::new(|_| Ok(Vec::new())))),
            block_index_calls: Arc::new(Default::default()),
        }
    }

    pub(crate) fn set_block_index_handler(
        &self,
        handler: impl Fn(BlockIndexQuery) -> Result<Vec<BlockIndexItem>> + Send + Sync + 'static,
    ) {
        let mut guard = self.block_index_handler.write().expect("to unlock handler");
        *guard = Box::new(handler);
    }
}

#[async_trait::async_trait]
impl ApiClient for ApiClientStub {
    async fn get_transaction(
        &self,
        _peer: SocketAddr,
        tx_id: H256,
    ) -> Result<IrysTransactionResponse> {
        Ok(self
            .txs
            .get(&tx_id)
            .ok_or(eyre!("Transaction {} not found in stub API client", tx_id))?
            .clone()
            .into())
    }

    async fn post_transaction(
        &self,
        _api_address: SocketAddr,
        _transaction: IrysTransactionHeader,
    ) -> Result<()> {
        Ok(())
    }

    async fn get_transactions(
        &self,
        peer: SocketAddr,
        tx_ids: &[H256],
    ) -> Result<Vec<IrysTransactionResponse>> {
        debug!("Fetching {} transactions from peer {}", tx_ids.len(), peer);
        let mut results = Vec::with_capacity(tx_ids.len());

        for &tx_id in tx_ids {
            let result = self.get_transaction(peer, tx_id).await?;
            results.push(result);
        }

        Ok(results)
    }

    async fn post_version(
        &self,
        _api_address: SocketAddr,
        _version: VersionRequest,
    ) -> Result<PeerResponse> {
        Ok(PeerResponse::Accepted(AcceptedResponse::default())) // Mock re sponse
    }

    async fn get_block_by_hash(
        &self,
        _peer: SocketAddr,
        _block_hash: H256,
    ) -> Result<Option<CombinedBlockHeader>> {
        Ok(None)
    }

    async fn get_block_index(
        &self,
        _peer: SocketAddr,
        block_index_query: BlockIndexQuery,
    ) -> Result<Vec<BlockIndexItem>> {
        self.block_index_calls
            .write()
            .expect("To unlock calls")
            .push(block_index_query.clone());
        let handler = self.block_index_handler.read().expect("to unlock response");
        handler(block_index_query)
    }
}

impl Default for ApiClientStub {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct GossipServiceTestFixture {
    pub gossip_port: u16,
    pub api_port: u16,
    pub execution: RethPeerInfo,
    pub db: DatabaseProvider,
    pub mining_address: Address,
    pub mempool_stub: MempoolStub,
    pub peer_list: Addr<PeerListServiceWithClient<ApiClientStub, MockRethServiceActor>>,
    pub mempool_txs: Arc<RwLock<Vec<IrysTransactionHeader>>>,
    pub mempool_chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub discovery_blocks: Arc<RwLock<Vec<IrysBlockHeader>>>,
    pub api_client_stub: ApiClientStub,
    // Tets need the task manager to be stored somewhere
    #[allow(dead_code)]
    pub task_manager: TaskManager,
    pub task_executor: TaskExecutor,
}

impl Default for GossipServiceTestFixture {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MockRethServiceActor {}

impl Actor for MockRethServiceActor {
    type Context = Context<Self>;
}

impl Handler<RethPeerInfo> for MockRethServiceActor {
    type Result = eyre::Result<()>;

    fn handle(&mut self, _msg: RethPeerInfo, _ctx: &mut Self::Context) -> Self::Result {
        Ok(())
    }
}

impl GossipServiceTestFixture {
    /// # Panics
    /// Can panic
    #[must_use]
    pub(crate) fn new() -> Self {
        let temp_dir = setup_tracing_and_temp_dir(Some("gossip_test_fixture"), false);
        let gossip_port = random_free_port();
        let config = NodeConfig::testnet().into();
        let api_port = random_free_port();
        let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir");
        let db = DatabaseProvider(Arc::new(db_env));

        let mock_reth_service = MockRethServiceActor {};
        let reth_service_addr = mock_reth_service.start();

        let peer_service = PeerListServiceWithClient::new_with_custom_api_client(
            db.clone(),
            &config,
            ApiClientStub::new(),
            reth_service_addr,
        );
        let peer_list = peer_service.start();

        let (gossip_sender, _rx) = mpsc::channel(100);

        let mempool_stub = MempoolStub::new(gossip_sender.clone());
        let mempool_txs = Arc::clone(&mempool_stub.txs);
        let mempool_chunks = Arc::clone(&mempool_stub.chunks);

        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::new(RwLock::new(Vec::new())),
            internal_message_bus: gossip_sender,
        };
        let discovery_blocks = Arc::clone(&block_discovery_stub.blocks);

        let tokio_runtime = tokio::runtime::Handle::current();

        let task_manager = TaskManager::new(tokio_runtime);
        let task_executor = task_manager.executor();

        Self {
            // temp_dir,
            gossip_port,
            api_port,
            execution: RethPeerInfo::default(),
            db,
            mining_address: Address::random(),
            mempool_stub,
            peer_list,
            // block_discovery_stub,
            mempool_txs,
            mempool_chunks,
            discovery_blocks,
            api_client_stub: ApiClientStub::new(),
            task_manager,
            task_executor,
        }
    }

    /// # Panics
    /// Can panic
    pub(crate) async fn run_service(
        &mut self,
    ) -> (ServiceHandleWithShutdownSignal, mpsc::Sender<GossipData>) {
        let (gossip_service, internal_message_bus) = P2PService::new(self.mining_address);
        let gossip_listener = TcpListener::bind(
            format!("127.0.0.1:{}", self.gossip_port)
                .parse::<SocketAddr>()
                .expect("Valid address"),
        )
        .expect("To bind");

        let mempool_stub = MempoolStub::new(internal_message_bus.clone());
        self.mempool_txs = Arc::clone(&mempool_stub.txs);
        self.mempool_chunks = Arc::clone(&mempool_stub.chunks);

        self.mempool_stub = mempool_stub.clone();

        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::clone(&self.discovery_blocks),
            internal_message_bus: internal_message_bus.clone(),
        };

        let (vdf_tx, _vdf_rx) = tokio::sync::mpsc::channel::<BroadcastMiningSeed>(1);

        let service_handle = gossip_service
            .run(
                mempool_stub,
                block_discovery_stub,
                self.api_client_stub.clone(),
                &self.task_executor,
                self.peer_list.clone().into(),
                self.db.clone(),
                vdf_tx,
                gossip_listener,
            )
            .expect("failed to run gossip service");

        (service_handle, internal_message_bus)
    }

    #[must_use]
    pub(crate) fn create_default_peer_entry(&self) -> PeerListItem {
        PeerListItem {
            reputation_score: PeerScore::new(50),
            response_time: 0,
            address: PeerAddress {
                gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.gossip_port),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.api_port),
                execution: self.execution,
            },
            last_seen: 0,
            is_online: true,
        }
    }

    /// # Panics
    /// Can panic
    pub(crate) async fn add_peer(&self, other: &Self) {
        let peer = other.create_default_peer_entry();

        debug!(
            "Adding peer {:?} to gossip service {:?}",
            peer, self.gossip_port
        );

        self.peer_list
            .send(AddPeer {
                mining_addr: other.mining_address,
                peer: peer.clone(),
            })
            .await
            .expect("Adding peer failed");
    }

    /// # Panics
    /// Can panic
    pub(crate) async fn add_peer_with_reputation(&self, other: &Self, score: PeerScore) {
        let peer = PeerListItem {
            address: PeerAddress {
                gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), other.gossip_port),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), other.api_port),
                execution: other.execution,
            },
            reputation_score: score,
            is_online: true,
            ..PeerListItem::default()
        };
        self.peer_list
            .send(AddPeer {
                mining_addr: other.mining_address,
                peer: peer.clone(),
            })
            .await
            .expect("Adding peer failed");
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
pub(crate) fn generate_test_tx() -> IrysTransaction {
    let testnet_config = NodeConfig::testnet();
    let config = Config::new(testnet_config);
    let account1 = IrysSigner::random_signer(&config.consensus);
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
pub(crate) fn create_test_chunks(tx: &IrysTransaction) -> Vec<UnpackedChunk> {
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

struct FakeGossipDataHandler {
    on_block_data_request: Box<dyn Fn(BlockHash) -> bool + Send + Sync>,
}

impl FakeGossipDataHandler {
    fn new() -> Self {
        Self {
            on_block_data_request: Box::new(|_| false),
        }
    }

    fn call_on_block_data_request(&self, block_hash: BlockHash) -> bool {
        (self.on_block_data_request)(block_hash)
    }

    fn set_on_block_data_request(
        &mut self,
        on_block_data_request: Box<dyn Fn(BlockHash) -> bool + Send + Sync>,
    ) {
        self.on_block_data_request = on_block_data_request;
    }
}

pub(crate) struct FakeGossipServer {
    handler: Arc<RwLock<FakeGossipDataHandler>>,
}

impl Debug for FakeGossipServer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGossipServer").finish()
    }
}

impl FakeGossipServer {
    pub(crate) fn new() -> Self {
        Self {
            handler: Arc::new(RwLock::new(FakeGossipDataHandler::new())),
        }
    }

    pub(crate) fn spawn(self) -> SocketAddr {
        let (server_handle, fake_peer_gossip_addr) =
            self.run(SocketAddr::from(([127, 0, 0, 1], 0)));
        tokio::spawn(server_handle);
        fake_peer_gossip_addr
    }

    pub(crate) fn set_on_block_data_request(
        &self,
        on_block_data_request: impl Fn(BlockHash) -> bool + Send + Sync + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_block_data_request(Box::new(on_block_data_request));
    }

    /// Runs the fake server, returns the address on which the server has started, as well
    /// as the server handle
    pub(crate) fn run(&self, address: SocketAddr) -> (Server, SocketAddr) {
        let handler = self.handler.clone();
        let server = HttpServer::new(move || {
            let handler = handler.clone();
            App::new()
                .app_data(web::Data::new(handler.clone()))
                .wrap(middleware::Logger::new("%r %s %D ms"))
                .service(web::resource("/gossip/get_data").route(web::post().to(handle_get_data)))
                .default_service(web::to(|| async {
                    warn!("Request hit default handler - check your route paths");
                    HttpResponse::NotFound()
                        .content_type("application/json")
                        .json(false)
                }))
        })
        .workers(1)
        .shutdown_timeout(5)
        .keep_alive(actix_web::http::KeepAlive::Disabled)
        .bind(address)
        .expect("to bind");

        let addr = server.addrs()[0].clone();
        let server = server.run();
        (server, addr)
    }
}

async fn handle_get_data(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequest>>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    warn!("Fake server got request: {:?}", data_request.data);

    match handler.read() {
        Ok(handler) => match data_request.data {
            GossipDataRequest::Block(block_hash) => {
                let res = handler.call_on_block_data_request(block_hash);
                warn!(
                    "Block data request for hash {:?}, response: {}",
                    block_hash.0.to_base58(),
                    res
                );
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(res)
            }
            GossipDataRequest::Transaction(transaction_hash) => {
                warn!("Transaction request for hash {:?}", transaction_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(false)
            }
        },
        Err(e) => {
            warn!("Failed to acquire read lock on handler: {}", e);
            HttpResponse::InternalServerError()
                .content_type("application/json")
                .json("Failed to process request")
        }
    }
}
