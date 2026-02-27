use crate::peer_network_service::spawn_peer_network_service_with_client;
use crate::types::GossipResponse;
use crate::{
    BlockPool, BlockStatusProvider, GossipCache, GossipClient, GossipDataHandler, P2PService,
    ServiceHandleWithShutdownSignal, SyncChainServiceMessage,
};
use actix_web::dev::Server;
use actix_web::{middleware, web, App, HttpResponse, HttpServer};
use async_trait::async_trait;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use eyre::Result;
use futures::{future, FutureExt as _};
use irys_actors::block_discovery::BlockDiscoveryError;
use irys_actors::mempool_guard::MempoolReadGuard;
use irys_actors::mempool_service::{create_state, AtomicMempoolState, TxIngressError, TxReadError};
use irys_actors::services::ServiceSenders;
use irys_actors::{block_discovery::BlockDiscoveryFacade, MempoolFacade};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::execution_payload_cache::{ExecutionPayloadCache, RethBlockProvider};
use irys_domain::{BlockIndex, BlockIndexReadGuard, BlockTree, BlockTreeReadGuard, PeerList};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::tempfile::TempDir;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::irys::IrysSigner;
use irys_types::v1::GossipDataRequestV1;
use irys_types::v2::{GossipBroadcastMessageV2, GossipDataRequestV2, GossipDataV2};
use irys_types::IrysAddress;
use irys_types::{
    Base64, BlockHash, BlockIndexItem, BlockIndexQuery, CommitmentTransaction, Config,
    DataTransaction, DataTransactionHeader, DatabaseProvider, GossipRequest, IrysBlockHeader,
    IrysPeerId, MempoolConfig, NodeConfig, NodeInfo, PeerAddress, PeerListItem, PeerNetworkSender,
    PeerScore, ProtocolVersion, RethPeerInfo, SealedBlock, SendTraced as _, TokioServiceHandle,
    Traced, TxChunkOffset, TxKnownStatus, UnpackedChunk, H256,
};
use irys_utils::circuit_breaker::CircuitBreakerConfig;
use irys_vdf::state::{VdfState, VdfStateReadonly};
use reth_tasks::TaskExecutor;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

#[derive(Clone, Debug)]
pub(crate) struct MempoolStub {
    pub txs: Arc<RwLock<Vec<DataTransactionHeader>>>,
    pub chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub internal_message_bus: mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    pub mempool_state: AtomicMempoolState,
}

impl MempoolStub {
    #[must_use]
    pub(crate) fn new(
        internal_message_bus: mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
        mempool_state: AtomicMempoolState,
    ) -> Self {
        Self {
            txs: Arc::default(),
            chunks: Arc::default(),
            internal_message_bus,
            mempool_state,
        }
    }
}

#[async_trait]
impl MempoolFacade for MempoolStub {
    async fn handle_data_transaction_ingress_api(
        &self,
        tx_header: DataTransactionHeader,
    ) -> std::result::Result<(), TxIngressError> {
        let already_exists = self
            .txs
            .read()
            .expect("to unlock mempool txs")
            .iter()
            .any(|tx| tx.eq_tx(&tx_header));

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
                .send_traced(GossipBroadcastMessageV2::from(tx_header))
                .expect("to send transaction");
        });

        Ok(())
    }

    async fn handle_data_transaction_ingress_gossip(
        &self,
        tx_header: DataTransactionHeader,
    ) -> std::result::Result<(), TxIngressError> {
        self.handle_data_transaction_ingress_api(tx_header).await
    }

    async fn handle_commitment_transaction_ingress_api(
        &self,
        _tx_header: CommitmentTransaction,
    ) -> std::result::Result<(), TxIngressError> {
        Ok(())
    }

    async fn handle_commitment_transaction_ingress_gossip(
        &self,
        _tx_header: CommitmentTransaction,
    ) -> std::result::Result<(), TxIngressError> {
        Ok(())
    }

    async fn is_known_data_transaction(
        &self,
        tx_id: H256,
    ) -> std::result::Result<TxKnownStatus, TxReadError> {
        if self
            .txs
            .read()
            .expect("to read txs")
            .iter()
            .any(|message| message.id == tx_id)
        {
            Ok(TxKnownStatus::Valid)
        } else {
            Ok(TxKnownStatus::Unknown)
        }
    }

    async fn is_known_commitment_transaction(
        &self,
        tx_id: H256,
    ) -> std::result::Result<TxKnownStatus, TxReadError> {
        if self
            .txs
            .read()
            .expect("to read txs")
            .iter()
            .any(|message| message.id == tx_id)
        {
            Ok(TxKnownStatus::Valid)
        } else {
            Ok(TxKnownStatus::Unknown)
        }
    }

    async fn remove_from_blacklist(&self, _tx_ids: Vec<H256>) -> eyre::Result<()> {
        Ok(())
    }

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<IrysAddress> {
        HashSet::new()
    }

    async fn update_stake_and_pledge_whitelist(
        &self,
        _new_whitelist: HashSet<IrysAddress>,
    ) -> Result<()> {
        Ok(())
    }

    async fn get_internal_read_guard(&self) -> MempoolReadGuard {
        MempoolReadGuard::new(self.mempool_state.clone())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BlockDiscoveryStub {
    pub blocks: Arc<RwLock<Vec<Arc<IrysBlockHeader>>>>,
    pub internal_message_bus: Option<mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>>,
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
        block: Arc<SealedBlock>,
        _skip_vdf: bool,
    ) -> std::result::Result<(), BlockDiscoveryError> {
        let header = Arc::clone(block.header());
        self.block_status_provider
            .add_block_to_index_and_tree_for_testing(&header);
        self.blocks
            .write()
            .expect("to unlock blocks")
            .push(header.clone());

        let sender = self.internal_message_bus.clone();

        if let Some(sender) = sender {
            // Pretend that we've validated the block and we're ready to gossip it
            tokio::runtime::Handle::current().spawn(async move {
                sender
                    .send_traced(GossipBroadcastMessageV2::from(header))
                    .expect("to send block");
            });
        }

        Ok(())
    }
}

pub(crate) struct GossipServiceTestFixture {
    pub gossip_port: u16,
    pub api_port: u16,
    pub execution: RethPeerInfo,
    pub db: DatabaseProvider,
    pub mining_address: IrysAddress,
    pub mempool_stub: MempoolStub,
    #[expect(dead_code)]
    pub peer_network_handle: TokioServiceHandle,
    pub peer_list: PeerList,
    pub mempool_txs: Arc<RwLock<Vec<DataTransactionHeader>>>,
    pub mempool_chunks: Arc<RwLock<Vec<UnpackedChunk>>>,
    pub discovery_blocks: Arc<RwLock<Vec<Arc<IrysBlockHeader>>>>,
    pub mempool_state: AtomicMempoolState,
    pub task_executor: TaskExecutor,
    pub block_status_provider: BlockStatusProvider,
    pub execution_payload_provider: ExecutionPayloadCache,
    pub config: Config,
    pub service_senders: ServiceSenders,
    pub gossip_receiver: Option<mpsc::UnboundedReceiver<Traced<GossipBroadcastMessageV2>>>,
    pub _sync_rx: Option<UnboundedReceiver<SyncChainServiceMessage>>,
    pub sync_tx: UnboundedSender<SyncChainServiceMessage>,
    // needs to be held so the directory is removed correctly
    pub _temp_dir: TempDir,
}

impl GossipServiceTestFixture {
    /// # Panics
    /// Can panic
    #[must_use]
    pub(crate) fn new() -> Self {
        let temp_dir = setup_tracing_and_temp_dir(Some("gossip_test_fixture"), false);
        let gossip_port = random_free_port();
        let api_port = random_free_port();

        warn!("Random port for gossip: {}", gossip_port);
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = temp_dir.path().to_path_buf();
        node_config.gossip.public_port = gossip_port;
        node_config.gossip.bind_port = gossip_port;
        node_config.http.public_port = gossip_port;
        node_config.http.bind_port = gossip_port;
        let random_signer = IrysSigner::random_signer(&node_config.consensus_config());
        node_config.mining_key = random_signer.signer;
        // Generate a distinct peer_id for this test fixture
        // peer_id is separate from mining_address in V2
        let config = Config::new_with_random_peer_id(node_config);

        let db_env = open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir");
        let db = DatabaseProvider(Arc::new(db_env));

        let (service_senders, service_receivers) =
            irys_actors::test_helpers::build_test_service_senders();
        let vdf_fast_forward_rx = service_receivers.vdf_fast_forward;
        let gossip_broadcast_rx = service_receivers.gossip_broadcast;
        let block_tree_rx = service_receivers.block_tree;
        let chunk_ingress_rx = service_receivers.chunk_ingress;

        let (sender, receiver) = PeerNetworkSender::new_with_receiver();

        let tokio_runtime = tokio::runtime::Handle::current();
        let reth_peer_sender = Arc::new(|peer_info: RethPeerInfo| {
            let _ = peer_info;
            future::ready(()).boxed()
        });

        let (peer_network_handle, peer_list) = spawn_peer_network_service_with_client(
            db.clone(),
            &config,
            reth_peer_sender,
            receiver,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
            tokio_runtime,
        );

        let mempool_config = MempoolConfig::testing();
        let state = create_state(&mempool_config, &[]);
        let mempool_state = AtomicMempoolState::new(state);

        let mempool_stub = MempoolStub::new(
            service_senders.gossip_broadcast.clone(),
            mempool_state.clone(),
        );
        let mempool_txs = Arc::clone(&mempool_stub.txs);
        let mempool_chunks = Arc::clone(&mempool_stub.chunks);

        let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config, db.clone());
        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::new(RwLock::new(Vec::new())),
            internal_message_bus: Some(service_senders.gossip_broadcast.clone()),
            block_status_provider: block_status_provider_mock.clone(),
        };
        let discovery_blocks = Arc::clone(&block_discovery_stub.blocks);

        let task_executor = TaskExecutor::test();

        let mocked_execution_payloads = Arc::new(RwLock::new(HashMap::new()));
        let execution_payload_provider = ExecutionPayloadCache::new(
            peer_list.clone(),
            RethBlockProvider::Mock(mocked_execution_payloads),
        );

        let vdf_state_stub =
            VdfStateReadonly::new(Arc::new(RwLock::new(VdfState::new(0, 0, None))));

        let vdf_state = vdf_state_stub;
        let mut vdf_receiver = vdf_fast_forward_rx;
        tokio::spawn(async move {
            loop {
                match vdf_receiver.recv().await {
                    Some(traced_step) => {
                        let (step, _parent_span) = traced_step.into_parts();
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

        let mut block_tree_receiver = block_tree_rx;
        tokio::spawn(async move {
            while let Some(message) = block_tree_receiver.recv().await {
                debug!("Received BlockTreeServiceMessage: {:?}", message);
            }
            debug!("BlockTreeServiceMessage channel closed");
        });

        // Consume chunk ingress messages, storing received chunks
        spawn_test_chunk_ingress_consumer(chunk_ingress_rx, Some(Arc::clone(&mempool_chunks)));

        let (sync_tx, sync_rx) = mpsc::unbounded_channel();

        Self {
            _temp_dir: temp_dir,
            gossip_port,
            api_port,
            execution: RethPeerInfo::default(),
            db,
            mining_address: config.node_config.miner_address(),
            mempool_stub,
            peer_network_handle,
            peer_list,
            mempool_txs,
            mempool_chunks,
            discovery_blocks,
            mempool_state,
            task_executor,
            block_status_provider: block_status_provider_mock,
            execution_payload_provider,
            config,
            service_senders,
            gossip_receiver: Some(gossip_broadcast_rx),
            sync_tx,
            _sync_rx: Some(sync_rx),
        }
    }

    /// # Panics
    /// Can panic
    pub(crate) fn run_service(
        &mut self,
    ) -> (
        ServiceHandleWithShutdownSignal,
        mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    ) {
        let gossip_service = P2PService::new(
            self.mining_address,
            self.config.peer_id(),
            self.gossip_receiver.take().expect("to take receiver"),
        );
        info!("Starting gossip service on port {}", self.gossip_port);
        let gossip_listener = TcpListener::bind(
            format!("127.0.0.1:{}", self.gossip_port)
                .parse::<SocketAddr>()
                .expect("Valid address"),
        )
        .expect("To bind");

        let mempool_stub = self.mempool_stub.clone();

        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::clone(&self.discovery_blocks),
            internal_message_bus: Some(self.service_senders.gossip_broadcast.clone()),
            block_status_provider: self.block_status_provider.clone(),
        };

        let peer_list = self.peer_list.clone();
        let execution_payload_provider = self.execution_payload_provider.clone();

        let gossip_broadcast = self.service_senders.gossip_broadcast.clone();

        let genesis = self.block_status_provider.genesis_header();
        gossip_service.sync_state.finish_sync();
        let (server, server_handle, broadcast_task_handle, _block_pool, _data_handler) =
            gossip_service
                .run(
                    mempool_stub,
                    block_discovery_stub,
                    &self.task_executor,
                    peer_list,
                    self.db.clone(),
                    gossip_listener,
                    self.block_status_provider.clone(),
                    execution_payload_provider,
                    self.config.clone(),
                    self.service_senders.clone(),
                    self.sync_tx.clone(),
                    MempoolReadGuard::new(self.mempool_state.clone()),
                    BlockIndexReadGuard::new(BlockIndex::new_for_testing(self.db.clone())),
                    BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
                        &genesis,
                        self.config.consensus.clone(),
                    )))),
                    std::time::Instant::now(),
                )
                .expect("failed to run the gossip service");

        let service_handle = crate::spawn_p2p_server_watcher_task(
            server,
            server_handle,
            broadcast_task_handle,
            &self.task_executor,
        );

        (service_handle, gossip_broadcast)
    }

    #[must_use]
    pub(crate) fn create_default_peer_entry(&self) -> PeerListItem {
        PeerListItem {
            peer_id: self.config.peer_id(),
            mining_address: self.config.node_config.miner_address(),
            reputation_score: PeerScore::new(50),
            response_time: 0,
            address: PeerAddress {
                gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.gossip_port),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.api_port),
                execution: self.execution,
            },
            last_seen: 0,
            is_online: true,
            protocol_version: ProtocolVersion::default(),
        }
    }

    /// # Panics
    /// Can panic
    pub(crate) fn add_peer(&self, other: &Self) {
        let peer = other.create_default_peer_entry();
        debug!(
            "Adding peer {:?}: {:?} to gossip service {:?}",
            other.mining_address, peer, self.gossip_port
        );

        self.peer_list.add_or_update_peer(peer, true);
    }

    pub(crate) async fn add_tx_to_mempool(&self, tx: DataTransactionHeader) {
        self.mempool_state
            .insert_tx_and_mark_valid(&tx)
            .await
            .expect("to insert tx");
    }

    /// Persist a block header to the fixture's MDBX database so that
    /// `block_header_lookup::get_block_header` (block tree → DB fallback) can find it.
    pub(crate) fn persist_block_header_to_db(&self, block: &IrysBlockHeader) {
        use irys_database::{db::IrysDatabaseExt as _, insert_block_header};
        self.db
            .0
            .update_eyre(|tx| insert_block_header(tx, block))
            .expect("to persist block header to DB");
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
    let config = Config::new_with_random_peer_id(testing_config);
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
        Box<dyn Fn(GossipDataRequestV2) -> GossipResponse<Option<GossipDataV2>> + Send + Sync>,
    on_info_request: Box<dyn Fn() -> GossipResponse<NodeInfo> + Send + Sync>,
    on_block_index_request:
        Box<dyn Fn(BlockIndexQuery) -> GossipResponse<Vec<BlockIndexItem>> + Send + Sync>,
}

impl FakeGossipDataHandler {
    fn new() -> Self {
        Self {
            on_block_data_request: Box::new(|_| GossipResponse::Accepted(false)),
            on_pull_data_request: Box::new(|_| GossipResponse::Accepted(None)),
            on_info_request: Box::new(|| GossipResponse::Accepted(NodeInfo::default())),
            on_block_index_request: Box::new(|_| GossipResponse::Accepted(Vec::new())),
        }
    }

    fn call_on_block_data_request(&self, block_hash: BlockHash) -> GossipResponse<bool> {
        (self.on_block_data_request)(block_hash)
    }

    fn call_on_pull_data_request(
        &self,
        data_request: GossipDataRequestV2,
    ) -> GossipResponse<Option<GossipDataV2>> {
        (self.on_pull_data_request)(data_request)
    }

    fn call_on_info_request(&self) -> GossipResponse<NodeInfo> {
        (self.on_info_request)()
    }

    fn call_on_block_index_request(
        &self,
        query: BlockIndexQuery,
    ) -> GossipResponse<Vec<BlockIndexItem>> {
        (self.on_block_index_request)(query)
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
            dyn Fn(GossipDataRequestV2) -> GossipResponse<Option<GossipDataV2>> + Send + Sync,
        >,
    ) {
        self.on_pull_data_request = on_pull_data_request;
    }

    fn set_on_info_request(
        &mut self,
        on_info_request: Box<dyn Fn() -> GossipResponse<NodeInfo> + Send + Sync>,
    ) {
        self.on_info_request = on_info_request;
    }

    fn set_on_block_index_request(
        &mut self,
        on_block_index_request: Box<
            dyn Fn(BlockIndexQuery) -> GossipResponse<Vec<BlockIndexItem>> + Send + Sync,
        >,
    ) {
        self.on_block_index_request = on_block_index_request;
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
        on_pull_data_request: impl Fn(GossipDataRequestV2) -> GossipResponse<Option<GossipDataV2>>
            + Send
            + Sync
            + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_pull_data_request(Box::new(on_pull_data_request));
    }

    pub(crate) fn set_on_info_request(
        &self,
        on_info_request: impl Fn() -> GossipResponse<NodeInfo> + Send + Sync + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_info_request(Box::new(on_info_request));
    }

    pub(crate) fn set_on_block_index_request(
        &self,
        on_block_index_request: impl Fn(BlockIndexQuery) -> GossipResponse<Vec<BlockIndexItem>>
            + Send
            + Sync
            + 'static,
    ) {
        self.handler
            .write()
            .expect("to unlock handler")
            .set_on_block_index_request(Box::new(on_block_index_request));
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
                .service(
                    web::resource("/gossip/v2/get_data").route(web::post().to(handle_get_data_v2)),
                )
                .service(
                    web::resource("/gossip/v2/pull_data")
                        .route(web::post().to(handle_pull_data_v2)),
                )
                .service(web::resource("/gossip/info").route(web::get().to(handle_info)))
                .service(
                    web::resource("/gossip/block-index").route(web::get().to(handle_block_index)),
                )
                .service(
                    web::resource("/gossip/protocol_version")
                        .route(web::get().to(handle_protocol_version)),
                )
                .service(web::resource("/gossip/health").route(web::get().to(handle_health)))
                .default_service(web::to(|| async {
                    warn!("Request hit default handler - check your route paths");
                    HttpResponse::NotFound()
                        .content_type("application/json")
                        .json(GossipResponse::Accepted(false))
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

async fn handle_get_data_v2(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequestV2>>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    warn!("Fake server got request: {:?}", data_request.data);

    match handler.read() {
        Ok(handler) => match &data_request.data {
            GossipDataRequestV2::BlockHeader(block_hash) => {
                let res = handler.call_on_block_data_request(*block_hash);
                warn!(
                    "Block data request for hash {:?}, response: {:?}",
                    block_hash, res
                );
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(res)
            }
            GossipDataRequestV2::BlockBody(block_hash) => {
                warn!("Block body request for hash {:?}", block_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(GossipResponse::Accepted(false))
            }
            GossipDataRequestV2::ExecutionPayload(evm_block_hash) => {
                warn!("Execution payload request for hash {:?}", evm_block_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(GossipResponse::Accepted(true))
            }
            GossipDataRequestV2::Chunk(chunk_hash) => {
                warn!("Chunk request for hash {:?}", chunk_hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(GossipResponse::Accepted(false))
            }
            GossipDataRequestV2::Transaction(hash) => {
                warn!("Transaction request for hash {:?}", hash);
                HttpResponse::Ok()
                    .content_type("application/json")
                    .json(GossipResponse::Accepted(false))
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

async fn handle_get_data(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequestV1>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let v1_request = data_request.0;
    let v2_data_request: GossipDataRequestV2 = v1_request.data.into();
    let v2_request = GossipRequest {
        miner_address: v1_request.miner_address,
        data: v2_data_request,
    };
    handle_get_data_v2(handler, web::Json(v2_request), req).await
}

async fn handle_pull_data_v2(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequestV2>>,
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

async fn handle_pull_data(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    data_request: web::Json<GossipRequest<GossipDataRequestV1>>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    let v1_request = data_request.0;
    let v2_data_request: GossipDataRequestV2 = v1_request.data.into();
    let v2_request = GossipRequest {
        miner_address: v1_request.miner_address,
        data: v2_data_request,
    };

    warn!(
        "Fake server got pull data v1 request: {:?}",
        v2_request.data
    );

    match handler.read() {
        Ok(handler) => {
            let data_request = v2_request.data;
            let response = handler.call_on_pull_data_request(data_request);
            // response is GossipResponse<Option<GossipDataV2>>

            let response_v1 = match response {
                GossipResponse::Accepted(maybe_data) => {
                    GossipResponse::Accepted(maybe_data.and_then(|d| d.to_v1()))
                }
                GossipResponse::Rejected(r) => GossipResponse::Rejected(r),
            };

            HttpResponse::Ok()
                .content_type("application/json")
                .json(response_v1)
        }
        Err(e) => {
            warn!("Failed to acquire read lock on handler: {}", e);
            HttpResponse::InternalServerError()
                .content_type("application/json")
                .json("Failed to process a request")
        }
    }
}

async fn handle_info(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    warn!("Fake server got info request");

    match handler.read() {
        Ok(handler) => {
            let response = handler.call_on_info_request();
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

async fn handle_protocol_version() -> HttpResponse {
    // Return both V1 and V2 support
    HttpResponse::Ok()
        .content_type("application/json")
        .json(vec![1_u32, 2_u32])
}

async fn handle_health() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .json(GossipResponse::Accepted(true))
}

async fn handle_block_index(
    handler: web::Data<Arc<RwLock<FakeGossipDataHandler>>>,
    query: web::Query<BlockIndexQuery>,
    _req: actix_web::HttpRequest,
) -> HttpResponse {
    warn!("Fake server got block index request: {:?}", query);

    match handler.read() {
        Ok(handler) => {
            let response = handler.call_on_block_index_request(query.into_inner());
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

/// Spawn a task that consumes chunk ingress messages from a receiver.
/// If `chunk_store` is `Some`, received chunks are pushed into it.
/// Replies `Ok(())` on any oneshot channel present in the message.
fn spawn_test_chunk_ingress_consumer(
    mut rx: UnboundedReceiver<Traced<irys_actors::ChunkIngressMessage>>,
    chunk_store: Option<Arc<RwLock<Vec<UnpackedChunk>>>>,
) {
    tokio::spawn(async move {
        use irys_actors::ChunkIngressMessage;
        while let Some(traced) = rx.recv().await {
            let (message, _parent_span) = traced.into_parts();
            match message {
                ChunkIngressMessage::IngestChunk(chunk, reply) => {
                    if let Some(ref store) = chunk_store {
                        store.write().expect("to unlock chunk store").push(chunk);
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                ChunkIngressMessage::IngestIngressProof(_proof, reply) => {
                    let _ = reply.send(Ok(()));
                }
                ChunkIngressMessage::ProcessPendingChunks(_) => {}
            }
        }
    });
}

pub(crate) fn data_handler_stub(
    config: &Config,
    peer_list_guard: &PeerList,
    db: DatabaseProvider,
    sync_state: ChainSyncState,
) -> Arc<GossipDataHandler<MempoolStub, BlockDiscoveryStub>> {
    let genesis_block = irys_testing_utils::new_mock_signed_header();
    let block_index = BlockIndex::new_for_testing(db.clone());
    let block_index_read_guard_stub = BlockIndexReadGuard::new(block_index);
    let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
    let block_tree_read_guard_stub = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

    let (service_senders, service_receivers) =
        irys_actors::test_helpers::build_test_service_senders();
    let gossip_tx = service_senders.gossip_broadcast.clone();
    let (sync_tx, _sync_rx) = mpsc::unbounded_channel();
    let mempool_config = MempoolConfig::testing();
    let state = create_state(&mempool_config, &[]);
    let mempool_state = AtomicMempoolState::new(state);
    let mempool_stub = MempoolStub::new(gossip_tx, mempool_state);
    let reth_block_mock_provider = RethBlockProvider::Mock(Arc::new(RwLock::new(HashMap::new())));
    let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config, db.clone());
    let block_discovery_stub = BlockDiscoveryStub {
        blocks: Arc::new(RwLock::new(Vec::new())),
        internal_message_bus: Some(service_senders.gossip_broadcast.clone()),
        block_status_provider: block_status_provider_mock,
    };
    let execution_payload_cache =
        ExecutionPayloadCache::new(peer_list_guard.clone(), reth_block_mock_provider);
    let chunk_ingress =
        irys_actors::chunk_ingress_service::facade::ChunkIngressFacadeImpl::from(&service_senders);
    // Keep the chunk_ingress receiver alive so the channel remains open.
    spawn_test_chunk_ingress_consumer(service_receivers.chunk_ingress, None);
    let block_pool_stub = Arc::new(BlockPool::new(
        db,
        block_discovery_stub,
        mempool_stub.clone(),
        sync_tx,
        sync_state.clone(),
        // Index guard, tree guard
        BlockStatusProvider::new(
            block_index_read_guard_stub.clone(),
            block_tree_read_guard_stub.clone(),
        ),
        // Reth service as a second argument
        execution_payload_cache.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    ));

    info!("Created GossipDataHandler stub");
    let consensus_config_hash = config.consensus.keccak256_hash();
    Arc::new(GossipDataHandler {
        mempool: mempool_stub,
        chunk_ingress,
        block_pool: block_pool_stub,
        cache: Arc::new(GossipCache::new()),
        gossip_client: GossipClient::with_circuit_breaker_config(
            Duration::from_millis(100000),
            IrysAddress::repeat_byte(2),
            IrysPeerId::from([0xAA_u8; 20]),
            CircuitBreakerConfig::testing(),
        ),
        peer_list: peer_list_guard.clone(),
        sync_state,
        execution_payload_cache,
        data_request_tracker: crate::rate_limiting::DataRequestTracker::new(),
        block_index: block_index_read_guard_stub,
        block_tree: block_tree_read_guard_stub,
        config: config.clone(),
        started_at: std::time::Instant::now(),
        consensus_config_hash,
    })
}

pub(crate) fn data_handler_with_stubbed_pool(
    peer_list_guard: &PeerList,
    sync_state: ChainSyncState,
    block_pool: Arc<BlockPool<BlockDiscoveryStub, MempoolStub>>,
    config: &Config,
    db: DatabaseProvider,
) -> Arc<GossipDataHandler<MempoolStub, BlockDiscoveryStub>> {
    let (service_senders, service_receivers) =
        irys_actors::test_helpers::build_test_service_senders();
    let gossip_tx = service_senders.gossip_broadcast.clone();
    let mempool_config = MempoolConfig::testing();
    let state = create_state(&mempool_config, &[]);
    let mempool_state = AtomicMempoolState::new(state);
    let mempool_stub = MempoolStub::new(gossip_tx, mempool_state);
    let reth_block_mock_provider = RethBlockProvider::Mock(Arc::new(RwLock::new(HashMap::new())));
    let execution_payload_cache =
        ExecutionPayloadCache::new(peer_list_guard.clone(), reth_block_mock_provider);

    let genesis_block = irys_testing_utils::new_mock_signed_header();
    let block_index = BlockIndex::new_for_testing(db);
    let block_index_read_guard_stub = BlockIndexReadGuard::new(block_index);
    let block_tree = BlockTree::new(&genesis_block, config.consensus.clone());
    let block_tree_read_guard_stub = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

    info!("Created GossipDataHandler stub");
    let chunk_ingress =
        irys_actors::chunk_ingress_service::facade::ChunkIngressFacadeImpl::from(&service_senders);
    // Keep the chunk_ingress receiver alive so the channel remains open.
    spawn_test_chunk_ingress_consumer(service_receivers.chunk_ingress, None);
    let consensus_config_hash = config.consensus.keccak256_hash();
    Arc::new(GossipDataHandler {
        mempool: mempool_stub,
        chunk_ingress,
        block_pool,
        cache: Arc::new(GossipCache::new()),
        gossip_client: GossipClient::with_circuit_breaker_config(
            Duration::from_millis(100000),
            IrysAddress::repeat_byte(2),
            IrysPeerId::from([0xAA_u8; 20]),
            CircuitBreakerConfig::testing(),
        ),
        peer_list: peer_list_guard.clone(),
        sync_state,
        execution_payload_cache,
        data_request_tracker: crate::rate_limiting::DataRequestTracker::new(),
        block_index: block_index_read_guard_stub,
        block_tree: block_tree_read_guard_stub,
        config: config.clone(),
        started_at: std::time::Instant::now(),
        consensus_config_hash,
    })
}

pub(crate) async fn wait_for_block(
    sync_state: &ChainSyncState,
    block_height: usize,
    timeout: Duration,
) {
    let start = tokio::time::Instant::now();
    loop {
        let highest_block = sync_state.highest_processed_block();
        if highest_block >= block_height {
            break;
        }

        if tokio::time::Instant::now().duration_since(start) > timeout {
            panic!("Timeout waiting for block at height {}", block_height);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
