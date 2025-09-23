use crate::peer_network_service::{GetPeerListGuard, PeerNetworkService};
use crate::types::GossipResponse;
use crate::{
    BlockPool, BlockStatusProvider, GossipCache, GossipClient, GossipDataHandler, P2PService,
    ServiceHandleWithShutdownSignal, SyncChainServiceMessage,
};
use actix::{Actor, Addr, Context, Handler};
use actix_web::dev::Server;
use actix_web::{middleware, web, App, HttpResponse, HttpServer};
use async_trait::async_trait;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use eyre::{eyre, Result};
use irys_actors::block_discovery::BlockDiscoveryError;
use irys_actors::services::ServiceSenders;
use irys_actors::{
    block_discovery::BlockDiscoveryFacade,
    mempool_service::{TxIngressError, TxReadError},
    ChunkIngressError, IngressProofError, MempoolFacade,
};
use irys_api_client::ApiClient;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::execution_payload_cache::{ExecutionPayloadCache, RethBlockProvider};
use irys_domain::{BlockIndex, BlockIndexReadGuard, BlockTree, BlockTreeReadGuard, PeerList};
use irys_primitives::Address;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::irys::IrysSigner;
use irys_types::{
    AcceptedResponse, Base64, BlockHash, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
    CommitmentTransaction, Config, DataTransaction, DataTransactionHeader, DatabaseProvider,
    GossipBroadcastMessage, GossipData, GossipDataRequest, GossipRequest, IngressProof,
    IrysBlockHeader, IrysTransactionResponse, NodeConfig, NodeInfo, PeerAddress, PeerListItem,
    PeerNetworkSender, PeerResponse, PeerScore, RethPeerInfo, TxChunkOffset, UnpackedChunk,
    VersionRequest, H256,
};
use irys_vdf::state::{VdfState, VdfStateReadonly};
use reth_tasks::{TaskExecutor, TaskManager};
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn, Span};

#[derive(Clone, Debug)]
pub(crate) struct MempoolStub {
    pub txs: Arc<RwLock<Vec<DataTransactionHeader>>>,
    pub chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub internal_message_bus: mpsc::UnboundedSender<GossipBroadcastMessage>,
    pub migrated_blocks: Arc<RwLock<Vec<Arc<IrysBlockHeader>>>>,
}

impl MempoolStub {
    #[must_use]
    pub(crate) fn new(internal_message_bus: mpsc::UnboundedSender<GossipBroadcastMessage>) -> Self {
        Self {
            txs: Arc::default(),
            chunks: Arc::default(),
            internal_message_bus,
            migrated_blocks: Arc::new(RwLock::new(Vec::new())),
        }
    }
}

#[async_trait]
impl MempoolFacade for MempoolStub {
    async fn handle_data_transaction_ingress(
        &self,
        tx_header: DataTransactionHeader,
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
                .send(GossipBroadcastMessage::from(tx_header))
                .expect("to send transaction");
        });

        Ok(())
    }

    async fn handle_commitment_transaction_ingress(
        &self,
        _tx_header: CommitmentTransaction,
    ) -> std::result::Result<(), TxIngressError> {
        Ok(())
    }

    async fn handle_ingest_ingress_proof(
        &self,
        _ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        Ok(())
    }

    async fn handle_chunk_ingress(
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
                .send(GossipBroadcastMessage::from(chunk))
                .expect("to send chunk");
        });

        Ok(())
    }

    async fn is_known_transaction(&self, tx_id: H256) -> std::result::Result<bool, TxReadError> {
        let exists = self
            .txs
            .read()
            .expect("to read txs")
            .iter()
            .any(|message| message.id == tx_id);
        Ok(exists)
    }

    async fn get_block_header(
        &self,
        _block_hash: H256,
        _include_chunk: bool,
    ) -> std::result::Result<Option<IrysBlockHeader>, TxReadError> {
        Ok(None)
    }

    async fn migrate_block(
        &self,
        irys_block_header: Arc<IrysBlockHeader>,
    ) -> std::result::Result<usize, TxIngressError> {
        self.migrated_blocks
            .write()
            .expect("to unlock migrated blocks")
            .push(irys_block_header);
        Ok(1)
    }

    async fn insert_poa_chunk(&self, _block_hash: H256, _chunk_data: Base64) -> Result<()> {
        Ok(())
    }

    async fn remove_from_blacklist(&self, _tx_ids: Vec<H256>) -> eyre::Result<()> {
        Ok(())
    }

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<Address> {
        HashSet::new()
    }

    async fn update_stake_and_pledge_whitelist(
        &self,
        _new_whitelist: HashSet<Address>,
    ) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockDiscoveryStub {
    pub blocks: Arc<RwLock<Vec<Arc<IrysBlockHeader>>>>,
    pub internal_message_bus: Option<mpsc::UnboundedSender<GossipBroadcastMessage>>,
    pub block_status_provider: BlockStatusProvider,
}

impl BlockDiscoveryStub {
    pub(crate) fn get_blocks(&self) -> Vec<Arc<IrysBlockHeader>> {
        self.blocks.read().unwrap().clone()
    }
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryStub {
    async fn handle_block(
        &self,
        block: Arc<IrysBlockHeader>,
        _skip_vdf: bool,
    ) -> std::result::Result<(), BlockDiscoveryError> {
        self.block_status_provider
            .add_block_to_index_and_tree_for_testing(&block);
        self.blocks
            .write()
            .expect("to unlock blocks")
            .push(block.clone());

        let sender = self.internal_message_bus.clone();

        if let Some(sender) = sender {
            // Pretend that we've validated the block and we're ready to gossip it
            tokio::runtime::Handle::current().spawn(async move {
                sender
                    .send(GossipBroadcastMessage::from(block))
                    .expect("to send block");
            });
        }

        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ApiClientStub {
    pub txs: HashMap<H256, DataTransactionHeader>,
    pub block_index_handler: Arc<
        RwLock<Box<dyn Fn(BlockIndexQuery) -> Result<Vec<BlockIndexItem>> + Send + Sync + 'static>>,
    >,
    pub block_index_calls: Arc<RwLock<Vec<BlockIndexQuery>>>,
    pub node_info_handler:
        Arc<RwLock<Box<dyn Fn(SocketAddr) -> Result<NodeInfo> + Send + Sync + 'static>>>,
}

impl ApiClientStub {
    pub(crate) fn new() -> Self {
        Self {
            txs: HashMap::new(),
            block_index_handler: Arc::new(RwLock::new(Box::new(|_| Ok(Vec::new())))),
            block_index_calls: Arc::new(Default::default()),
            node_info_handler: Arc::new(RwLock::new(Box::new(|_| Ok(NodeInfo::default())))),
        }
    }

    pub(crate) fn set_block_index_handler(
        &self,
        handler: impl Fn(BlockIndexQuery) -> Result<Vec<BlockIndexItem>> + Send + Sync + 'static,
    ) {
        let mut guard = self.block_index_handler.write().expect("to unlock handler");
        *guard = Box::new(handler);
    }

    pub(crate) fn set_node_info_handler(
        &self,
        handler: impl Fn(SocketAddr) -> Result<NodeInfo> + Send + Sync + 'static,
    ) {
        let mut guard = self
            .node_info_handler
            .write()
            .expect("to unlock node_info handler");
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
        _transaction: DataTransactionHeader,
    ) -> Result<()> {
        Ok(())
    }

    async fn post_commitment_transaction(
        &self,
        _peer: SocketAddr,
        _transaction: CommitmentTransaction,
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
        Ok(PeerResponse::Accepted(AcceptedResponse::default())) // Mock response
    }

    async fn get_block_by_hash(
        &self,
        _peer: SocketAddr,
        _block_hash: H256,
    ) -> Result<Option<CombinedBlockHeader>> {
        Ok(None)
    }

    async fn get_block_by_height(
        &self,
        _peer: SocketAddr,
        _block_height: u64,
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

    async fn node_info(&self, _peer: SocketAddr) -> Result<NodeInfo> {
        let handler = self
            .node_info_handler
            .read()
            .expect("to unlock node_info handler");
        handler(_peer)
    }
}

impl Default for ApiClientStub {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) type PeerListMock = Addr<PeerNetworkService<ApiClientStub, MockRethServiceActor>>;

pub(crate) struct GossipServiceTestFixture {
    pub gossip_port: u16,
    pub api_port: u16,
    pub execution: RethPeerInfo,
    pub db: DatabaseProvider,
    pub mining_address: Address,
    pub mempool_stub: MempoolStub,
    pub peer_list: PeerListMock,
    pub mempool_txs: Arc<RwLock<Vec<DataTransactionHeader>>>,
    pub mempool_chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub discovery_blocks: Arc<RwLock<Vec<Arc<IrysBlockHeader>>>>,
    pub api_client_stub: ApiClientStub,
    // Tets need the task manager to be stored somewhere
    #[expect(dead_code)]
    pub task_manager: TaskManager,
    pub task_executor: TaskExecutor,
    pub block_status_provider: BlockStatusProvider,
    pub execution_payload_provider: ExecutionPayloadCache,
    pub config: Config,
    pub service_senders: ServiceSenders,
    pub gossip_receiver: Option<mpsc::UnboundedReceiver<GossipBroadcastMessage>>,
    pub _sync_rx: Option<UnboundedReceiver<SyncChainServiceMessage>>,
    pub sync_tx: UnboundedSender<SyncChainServiceMessage>,
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
    pub(crate) async fn new() -> Self {
        let temp_dir = setup_tracing_and_temp_dir(Some("gossip_test_fixture"), false);
        let gossip_port = random_free_port();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = temp_dir.path().to_path_buf();
        let random_signer = IrysSigner::random_signer(&node_config.consensus_config());
        node_config.mining_key = random_signer.signer;
        let config = Config::new(node_config);

        let api_port = random_free_port();
        let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir");
        let db = DatabaseProvider(Arc::new(db_env));

        let mock_reth_service = MockRethServiceActor {};
        let reth_service_addr = mock_reth_service.start();

        let (service_senders, service_receivers) = ServiceSenders::new();

        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let peer_service = PeerNetworkService::new_with_custom_api_client(
            db.clone(),
            &config,
            ApiClientStub::new(),
            reth_service_addr,
            receiver,
            sender,
        );
        let peer_list = peer_service.start();
        let peer_list_data_guard = peer_list
            .send(GetPeerListGuard)
            .await
            .expect("to get peer list")
            .expect("to get peer list");

        let mempool_stub = MempoolStub::new(service_senders.gossip_broadcast.clone());
        let mempool_txs = Arc::clone(&mempool_stub.txs);
        let mempool_chunks = Arc::clone(&mempool_stub.chunks);

        let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config).await;
        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::new(RwLock::new(Vec::new())),
            internal_message_bus: Some(service_senders.gossip_broadcast.clone()),
            block_status_provider: block_status_provider_mock.clone(),
        };
        let discovery_blocks = Arc::clone(&block_discovery_stub.blocks);

        let tokio_runtime = tokio::runtime::Handle::current();

        let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config).await;

        let task_manager = TaskManager::new(tokio_runtime);
        let task_executor = task_manager.executor();

        let mocked_execution_payloads = Arc::new(RwLock::new(HashMap::new()));
        let execution_payload_provider = ExecutionPayloadCache::new(
            peer_list_data_guard,
            RethBlockProvider::Mock(mocked_execution_payloads),
        );

        let vdf_state_stub =
            VdfStateReadonly::new(Arc::new(RwLock::new(VdfState::new(0, 0, None))));

        let vdf_state = vdf_state_stub;
        let mut vdf_receiver = service_receivers.vdf_fast_forward;
        tokio::spawn(async move {
            loop {
                match vdf_receiver.recv().await {
                    Some(step) => {
                        debug!("Received VDF step: {:?}", step);
                        let state = vdf_state.into_inner_cloned();
                        let mut lock = state.write().unwrap();
                        lock.global_step = step.global_step_number;
                    }
                    None => {
                        debug!("VDF receiver channel closed");
                        break;
                    }
                }
            }
        });

        let mut block_tree_receiver = service_receivers.block_tree;
        tokio::spawn(async move {
            while let Some(message) = block_tree_receiver.recv().await {
                debug!("Received BlockTreeServiceMessage: {:?}", message);
            }
            debug!("BlockTreeServiceMessage channel closed");
        });

        let (sync_tx, sync_rx) = mpsc::unbounded_channel::<SyncChainServiceMessage>();

        Self {
            // temp_dir,
            gossip_port,
            api_port,
            execution: RethPeerInfo::default(),
            db,
            mining_address: config.node_config.miner_address(),
            mempool_stub,
            peer_list,
            // block_discovery_stub,
            mempool_txs,
            mempool_chunks,
            discovery_blocks,
            api_client_stub: ApiClientStub::new(),
            task_manager,
            task_executor,
            block_status_provider: block_status_provider_mock,
            execution_payload_provider,
            config,
            service_senders,
            gossip_receiver: Some(service_receivers.gossip_broadcast),
            sync_tx,
            _sync_rx: Some(sync_rx),
        }
    }

    /// # Panics
    /// Can panic
    pub(crate) async fn run_service(
        &mut self,
    ) -> (
        ServiceHandleWithShutdownSignal,
        mpsc::UnboundedSender<GossipBroadcastMessage>,
    ) {
        let gossip_service = P2PService::new(
            self.mining_address,
            self.gossip_receiver.take().expect("to take receiver"),
        );
        let gossip_listener = TcpListener::bind(
            format!("127.0.0.1:{}", self.gossip_port)
                .parse::<SocketAddr>()
                .expect("Valid address"),
        )
        .expect("To bind");

        let mempool_stub = MempoolStub::new(self.service_senders.gossip_broadcast.clone());
        self.mempool_txs = Arc::clone(&mempool_stub.txs);
        self.mempool_chunks = Arc::clone(&mempool_stub.chunks);

        self.mempool_stub = mempool_stub.clone();

        let block_status_provider_mock = BlockStatusProvider::mock(&self.config.node_config).await;
        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::clone(&self.discovery_blocks),
            internal_message_bus: Some(self.service_senders.gossip_broadcast.clone()),
            block_status_provider: block_status_provider_mock.clone(),
        };

        let peer_list = self
            .peer_list
            .send(GetPeerListGuard)
            .await
            .expect("to get peer list guard")
            .expect("to get peer list guard");
        let execution_payload_provider = self.execution_payload_provider.clone();

        let gossip_broadcast = self.service_senders.gossip_broadcast.clone();

        gossip_service.sync_state.finish_sync();
        let (service_handle, _block_pool, _data_handler) = gossip_service
            .run(
                mempool_stub,
                block_discovery_stub,
                self.api_client_stub.clone(),
                &self.task_executor,
                peer_list,
                self.db.clone(),
                gossip_listener,
                self.block_status_provider.clone(),
                execution_payload_provider,
                self.config.clone(),
                self.service_senders.clone(),
                self.sync_tx.clone(),
            )
            .expect("failed to run the gossip service");

        (service_handle, gossip_broadcast)
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
            "Adding peer {:?}: {:?} to gossip service {:?}",
            other.mining_address, peer, self.gossip_port
        );

        let peer_list_guard = self
            .peer_list
            .send(GetPeerListGuard)
            .await
            .expect("to get peer list guard")
            .expect("to get peer list guard");

        peer_list_guard.add_or_update_peer(other.mining_address, peer.clone(), true);
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
pub(crate) fn generate_test_tx() -> DataTransaction {
    let testing_config = NodeConfig::testing();
    let config = Config::new(testing_config);
    let account1 = IrysSigner::random_signer(&config.consensus);
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    // post a tx, mine a block
    let tx = account1
        .create_transaction(data_bytes, H256::zero())
        .expect("Failed to create transaction");
    account1
        .sign_transaction(tx)
        .expect("signing transaction failed")
}

#[must_use]
pub(crate) fn create_test_chunks(tx: &DataTransaction) -> Vec<UnpackedChunk> {
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
    on_block_data_request: Box<dyn Fn(BlockHash) -> GossipResponse<bool> + Send + Sync>,
    on_pull_data_request:
        Box<dyn Fn(GossipDataRequest) -> GossipResponse<Option<GossipData>> + Send + Sync>,
}

impl FakeGossipDataHandler {
    fn new() -> Self {
        Self {
            on_block_data_request: Box::new(|_| GossipResponse::Accepted(false)),
            on_pull_data_request: Box::new(|_| GossipResponse::Accepted(None)),
        }
    }

    fn call_on_block_data_request(&self, block_hash: BlockHash) -> GossipResponse<bool> {
        (self.on_block_data_request)(block_hash)
    }

    fn call_on_pull_data_request(
        &self,
        data_request: GossipDataRequest,
    ) -> GossipResponse<Option<GossipData>> {
        (self.on_pull_data_request)(data_request)
    }

    fn set_on_block_data_request(
        &mut self,
        on_block_data_request: Box<dyn Fn(BlockHash) -> GossipResponse<bool> + Send + Sync>,
    ) {
        self.on_block_data_request = on_block_data_request;
    }

    fn set_on_pull_data_request(
        &mut self,
        on_pull_data_request: Box<
            dyn Fn(GossipDataRequest) -> GossipResponse<Option<GossipData>> + Send + Sync,
        >,
    ) {
        self.on_pull_data_request = on_pull_data_request;
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
        on_block_data_request: impl Fn(BlockHash) -> GossipResponse<bool> + Send + Sync + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_block_data_request(Box::new(on_block_data_request));
    }

    pub(crate) fn set_on_pull_data_request(
        &self,
        on_pull_data_request: impl Fn(GossipDataRequest) -> GossipResponse<Option<GossipData>>
            + Send
            + Sync
            + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_pull_data_request(Box::new(on_pull_data_request));
    }

    /// Runs the fake server, returns the address on which the server has started, as well
    /// as the server handle
    pub(crate) fn run(&self, address: SocketAddr) -> (Server, SocketAddr) {
        let handler = self.handler.clone();
        let server = HttpServer::new(move || {
            let handler = handler.clone();
            App::new()
                .app_data(web::Data::new(handler))
                .wrap(middleware::Logger::new("%r %s %D ms"))
                .service(web::resource("/gossip/get_data").route(web::post().to(handle_get_data)))
                .service(web::resource("/gossip/pull_data").route(web::post().to(handle_pull_data)))
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

        let addr = server.addrs()[0];
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
                    "Block data request for hash {:?}, response: {:?}",
                    block_hash, res
                );
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(res)
            }
            GossipDataRequest::ExecutionPayload(evm_block_hash) => {
                warn!("Execution payload request for hash {:?}", evm_block_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(true)
            }
            GossipDataRequest::Chunk(chunk_hash) => {
                warn!("Chunk request for hash {:?}", chunk_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(false)
            }
        },
        Err(e) => {
            warn!("Failed to acquire read lock on handler: {}", e);
            HttpResponse::InternalServerError()
                .content_type("application/json")
                .json("Failed to process a request")
        }
    }
}

async fn handle_pull_data(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequest>>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    warn!("Fake server got pull data request: {:?}", data_request.data);

    match handler.read() {
        Ok(handler) => {
            let data_request = data_request.data.clone();
            let response = handler.call_on_pull_data_request(data_request);
            HttpResponse::Ok()
                .content_type("application/json")
                .json(response)
        }
        Err(e) => {
            warn!("Failed to acquire read lock on handler: {}", e);
            HttpResponse::InternalServerError()
                .content_type("application/json")
                .json("Failed to process a request")
        }
    }
}

pub(crate) async fn data_handler_stub<T: ApiClient>(
    config: &Config,
    peer_list_guard: &PeerList,
    db: DatabaseProvider,
    api_client_stub: T,
    sync_state: ChainSyncState,
) -> Arc<GossipDataHandler<MempoolStub, BlockDiscoveryStub, T>> {
    let genesis_block = IrysBlockHeader::new_mock_header();
    let block_index = BlockIndex::new(&config.node_config)
        .await
        .expect("expected to create a block index");
    let block_index_read_guard_stub = BlockIndexReadGuard::new(Arc::new(RwLock::new(block_index)));
    let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
    let block_tree_read_guard_stub = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

    let (service_senders, _service_receivers) = ServiceSenders::new();
    let gossip_tx = service_senders.gossip_broadcast.clone();
    let (sync_tx, _sync_rx) = mpsc::unbounded_channel();
    let mempool_stub = MempoolStub::new(gossip_tx);
    let reth_block_mock_provider = RethBlockProvider::Mock(Arc::new(RwLock::new(HashMap::new())));
    let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config).await;
    let block_discovery_stub = BlockDiscoveryStub {
        blocks: Arc::new(RwLock::new(Vec::new())),
        internal_message_bus: Some(service_senders.gossip_broadcast.clone()),
        block_status_provider: block_status_provider_mock,
    };
    let execution_payload_cache =
        ExecutionPayloadCache::new(peer_list_guard.clone(), reth_block_mock_provider.clone());
    let block_pool_stub = Arc::new(BlockPool::new(
        db.clone(),
        block_discovery_stub,
        mempool_stub.clone(),
        sync_tx,
        sync_state.clone(),
        // Index guard, tree guard
        BlockStatusProvider::new(block_index_read_guard_stub, block_tree_read_guard_stub),
        // Reth service as a second argument
        execution_payload_cache.clone(),
        config.clone(),
        service_senders,
    ));

    Arc::new(GossipDataHandler {
        mempool: mempool_stub,
        block_pool: block_pool_stub,
        cache: Arc::new(GossipCache::new()),
        api_client: api_client_stub.clone(),
        gossip_client: GossipClient::new(Duration::from_millis(100000), Address::repeat_byte(2)),
        peer_list: peer_list_guard.clone(),
        sync_state: sync_state.clone(),
        span: Span::current(),
        execution_payload_cache,
        data_request_tracker: crate::rate_limiting::DataRequestTracker::new(),
    })
}
