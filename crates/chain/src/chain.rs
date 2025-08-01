use crate::peer_utilities::{fetch_genesis_block, fetch_genesis_commitments};
use actix::{Actor as _, Addr, Arbiter, System, SystemRegistry};
use actix_web::dev::Server;
use base58::ToBase58 as _;
use irys_actors::block_discovery::{
    BlockDiscoveryMessage, BlockDiscoveryService, BlockDiscoveryServiceInner,
};
use irys_actors::block_tree_service::BlockTreeServiceMessage;
use irys_actors::broadcast_mining_service::MiningServiceBroadcaster;
use irys_actors::{
    block_discovery::BlockDiscoveryFacadeImpl,
    block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerCommand,
    block_tree_service::BlockTreeService,
    broadcast_mining_service::BroadcastMiningService,
    cache_service::ChunkCacheService,
    chunk_migration_service::ChunkMigrationService,
    mempool_service::{MempoolService, MempoolServiceFacadeImpl},
    mining::{MiningControl, PartitionMiningActor},
    packing::{PackingActor, PackingConfig, PackingRequest},
    reth_service::{
        BlockHashType, ForkChoiceUpdateMessage, GetPeeringInfoMessage, RethServiceActor,
    },
    services::ServiceSenders,
    validation_service::ValidationService,
};
use irys_actors::{ActorAddresses, BlockValidationTracker, DataSyncService, StorageModuleService};
use irys_api_client::IrysApiClient;
use irys_api_server::{create_listener, run_server, ApiState};
use irys_config::chain::chainspec::IrysChainSpecBuilder;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::db::RethDbWrapper;
use irys_database::{add_genesis_commitments, database, get_genesis_commitments, SystemLedger};
use irys_domain::{
    reth_provider, BlockIndex, BlockIndexReadGuard, BlockTreeReadGuard, ChunkProvider, ChunkType,
    EpochReplayData, ExecutionPayloadCache, IrysRethProvider, IrysRethProviderInner, PeerList,
    StorageModule, StorageModuleInfo, StorageModulesReadGuard,
};
use irys_p2p::{
    BlockPool, BlockStatusProvider, GetPeerListGuard, P2PService, PeerNetworkService,
    ServiceHandleWithShutdownSignal, SyncState,
};
use irys_price_oracle::{mock_oracle::MockOracle, IrysPriceOracle};
use irys_reth_node_bridge::irys_reth::payload::ShadowTxStore;
use irys_reth_node_bridge::node::{NodeProvider, RethNode, RethNodeHandle};
pub use irys_reth_node_bridge::node::{RethNodeAddOns, RethNodeProvider};
use irys_reth_node_bridge::signal::run_until_ctrl_c_or_channel_message;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_types::BlockHash;
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, ArbiterEnum, ArbiterHandle,
    CloneableJoinHandle, CommitmentTransaction, Config, IrysBlockHeader, NodeConfig, NodeMode,
    OracleConfig, PartitionChunkRange, PeerNetworkSender, PeerNetworkServiceMessage, ServiceSet,
    TokioServiceHandle, H256, U256,
};
use irys_vdf::vdf::run_vdf_for_genesis_block;
use irys_vdf::{
    state::{AtomicVdfState, VdfStateReadonly},
    vdf::run_vdf,
    VdfStep,
};
use reth::{
    chainspec::ChainSpec,
    tasks::{TaskExecutor, TaskManager},
};
use reth_db::Database as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use std::{
    net::TcpListener,
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::{self};
use tokio::sync::oneshot::{self};
use tracing::{debug, error, info, instrument, warn, Instrument as _, Span};

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    // todo replace this with `IrysRethNodeAdapter` but that requires quite a bit of refactoring
    pub reth_handle: RethNodeProvider,
    pub reth_node_adapter: IrysRethNodeAdapter,
    pub reth_db: RethDbWrapper,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub config: Config,
    pub reward_curve: Arc<HalvingCurve>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub block_tree_guard: BlockTreeReadGuard,
    pub vdf_steps_guard: VdfStateReadonly,
    pub service_senders: ServiceSenders,
    // Shutdown channels
    pub reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
    // Thread handles spawned by the start function
    pub reth_thread_handle: Option<CloneableJoinHandle<()>>,
    pub block_producer_inner: Arc<irys_actors::BlockProducerInner>,
    stop_guard: StopGuard,
    pub peer_list: PeerList,
    pub sync_state: SyncState,
    pub shadow_tx_store: ShadowTxStore,
    pub validation_enabled: Arc<AtomicBool>,
    pub block_pool: Arc<BlockPool<BlockDiscoveryFacadeImpl, MempoolServiceFacadeImpl>>,
    pub storage_modules_guard: StorageModulesReadGuard,
    pub mempool_pledge_provider: Arc<irys_actors::mempool_service::MempoolPledgeProvider>,
}

impl IrysNodeCtx {
    pub fn get_api_state(&self) -> ApiState {
        ApiState {
            mempool_service: self.service_senders.mempool.clone(),
            chunk_provider: self.chunk_provider.clone(),
            peer_list: self.peer_list.clone(),
            db: self.db.clone(),
            config: self.config.clone(),
            reth_provider: self.reth_handle.clone(),
            reth_http_url: self.reth_handle.rpc_server_handle().http_url().unwrap(),
            block_tree: self.block_tree_guard.clone(),
            block_index: self.block_index_guard.clone(),
            sync_state: self.sync_state.clone(),
            mempool_pledge_provider: self.mempool_pledge_provider.clone(),
        }
    }

    pub async fn stop(self) {
        let _ = self.stop_mining().await;
        debug!("Sending shutdown signal to reth thread");
        // Shutting down reth node will propagate to the main actor thread eventually
        let _ = self.reth_shutdown_sender.send(()).await;
        let _ = self.reth_thread_handle.unwrap().join();
        debug!("Main actor thread and reth thread stopped");
        self.stop_guard.mark_stopped();
    }

    pub fn get_http_port(&self) -> u16 {
        self.config.node_config.http.bind_port
    }

    /// Stop the VDF thread and send a message to all known partition actors to ignore any received VDF steps
    pub async fn stop_mining(&self) -> eyre::Result<()> {
        // stop the VDF thread
        self.stop_vdf().await?;
        self.set_partition_mining(false)
    }
    /// Start VDF thread and send a message to all known partition actors to begin mining when they receive a VDF step
    pub async fn start_mining(&self) -> eyre::Result<()> {
        // start the VDF thread
        self.start_vdf().await?;
        self.set_partition_mining(true)
    }
    // Send a custom control message to all known partition actors to enable/disable partition mining
    // does NOT modify the state of the  VDF thread!
    pub fn set_partition_mining(&self, should_mine: bool) -> eyre::Result<()> {
        // Send a custom control message to all known partition actors
        for part in &self.actor_addresses.partitions {
            part.try_send(MiningControl(should_mine))?;
        }
        Ok(())
    }
    // starts the VDF thread
    pub async fn start_vdf(&self) -> eyre::Result<()> {
        self.vdf_state(true).await
    }
    // stops the VDF thread
    pub async fn stop_vdf(&self) -> eyre::Result<()> {
        self.vdf_state(false).await
    }
    // sets the running state of the VDF thread
    pub async fn vdf_state(&self, running: bool) -> eyre::Result<()> {
        Ok(self.service_senders.vdf_mining.send(running).await?)
    }

    /// Sets whether the validation service should process incoming validation messages
    pub fn set_validation_enabled(&self, enabled: bool) {
        self.validation_enabled.store(enabled, Ordering::Relaxed);
    }
}

// Shared stop guard that can be cloned
#[derive(Debug)]
struct StopGuard(Arc<AtomicBool>);

impl StopGuard {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn mark_stopped(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        // Only check if this is the last reference to the guard
        if Arc::strong_count(&self.0) == 1 && !self.is_stopped() && !thread::panicking() {
            error!("\x1b[1;31m============================================================\x1b[0m");
            error!("\x1b[1;31mIrysNodeCtx must be stopped before all instances are dropped\x1b[0m");
            error!("\x1b[1;31m============================================================\x1b[0m");
        }
    }
}

impl Clone for StopGuard {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

async fn start_reth_node(
    task_executor: TaskExecutor,
    chainspec: ChainSpec,
    config: Config,
    sender: oneshot::Sender<RethNode>,
    latest_block: u64,
    shadow_tx_store: ShadowTxStore,
) -> eyre::Result<RethNodeHandle> {
    let random_ports = config.node_config.reth.use_random_ports;
    let (node_handle, _reth_node_adapter) = match irys_reth_node_bridge::node::run_node(
        Arc::new(chainspec.clone()),
        task_executor.clone(),
        config.node_config.clone(),
        latest_block,
        random_ports,
        shadow_tx_store.clone(),
    )
    .in_current_span()
    .await
    {
        Ok(handle) => handle,
        Err(e) => {
            error!("Restarting reth thread - reason: {:?}", &e);
            // One retry attempt
            irys_reth_node_bridge::node::run_node(
                Arc::new(chainspec.clone()),
                task_executor.clone(),
                config.node_config.clone(),
                latest_block,
                random_ports,
                shadow_tx_store,
            )
            .in_current_span()
            .await
            .expect("expected reth node to have started")
        }
    };

    debug!("Reth node started");

    sender.send(node_handle.node.clone()).map_err(|e| {
        eyre::eyre!(
            "Failed to send reth node handle to main actor thread: {:?}",
            &e
        )
    })?;

    Ok(node_handle)
}

/// Builder pattern for configuring and bootstrapping an Irys blockchain node.
pub struct IrysNode {
    pub config: Config,
    pub http_listener: TcpListener,
    pub gossip_listener: TcpListener,
}

impl IrysNode {
    /// Creates a new node builder instance.
    pub fn new(mut node_config: NodeConfig) -> eyre::Result<Self> {
        // we create the listener here so we know the port before we start passing around `config`
        let http_listener = create_listener(
            format!(
                "{}:{}",
                &node_config.http.bind_ip, &node_config.http.bind_port
            )
            .parse()
            .expect("A valid HTTP IP & port"),
        )?;
        let gossip_listener = create_listener(
            format!(
                "{}:{}",
                &node_config.gossip.bind_ip, &node_config.gossip.bind_port
            )
            .parse()
            .expect("A valid HTTP IP & port"),
        )?;
        let local_addr = http_listener
            .local_addr()
            .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?;
        let local_gossip = gossip_listener
            .local_addr()
            .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?;

        // if `config.port` == 0, the assigned port will be random (decided by the OS)
        // we re-assign the configuration with the actual port here.
        if node_config.http.bind_port == 0 {
            node_config.http.bind_port = local_addr.port();
        }

        // If the public port is not specified, use the same as the private one
        if node_config.http.public_port == 0 {
            node_config.http.public_port = node_config.http.bind_port;
        }

        if node_config.gossip.bind_port == 0 {
            node_config.gossip.bind_port = local_gossip.port();
        }

        if node_config.gossip.public_port == 0 {
            node_config.gossip.public_port = node_config.gossip.bind_port;
        }

        let config = Config::new(node_config);
        Ok(Self {
            config,
            http_listener,
            gossip_listener,
        })
    }

    async fn get_or_create_genesis_info(
        &self,
        node_mode: &NodeMode,
        genesis_block: IrysBlockHeader,
        irys_db: &DatabaseProvider,
        block_index: &BlockIndex,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        info!(miner_address = ?self.config.node_config.miner_address(), "Starting Irys Node: {:?}", node_mode);

        // Check if blockchain data already exists
        let has_existing_data = block_index.num_blocks() > 0;

        if has_existing_data {
            // CASE 1: Load existing genesis block and commitments from database
            return self.load_existing_genesis(irys_db, block_index);
        }

        // CASE 2: No existing data - handle based on node mode
        match node_mode {
            NodeMode::Genesis => {
                // Create a new genesis block for network initialization
                self.create_new_genesis_block(genesis_block.clone()).await
            }
            NodeMode::PeerSync | NodeMode::TrustedPeerSync => {
                // Fetch genesis data from trusted peer when joining network
                self.fetch_genesis_from_trusted_peer().await
            }
        }
    }

    // Helper methods to flatten the main function
    fn load_existing_genesis(
        &self,
        irys_db: &DatabaseProvider,
        block_index: &BlockIndex,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        // Get the genesis block hash from index
        let block_item = block_index
            .get_item(0)
            .expect("a block index item at index 0 in the block_index");

        // Retrieve genesis block header from database
        let tx = irys_db.tx().unwrap();
        let genesis_block = database::block_header_by_hash(&tx, &block_item.block_hash, false)
            .unwrap()
            .expect("Expect to find genesis block header in irys_db");

        // Find commitment ledger in system ledgers
        let commitment_ledger = genesis_block
            .system_ledgers
            .iter()
            .find(|e| e.ledger_id == SystemLedger::Commitment)
            .expect("Commitment ledger should exist in the genesis block");

        // Load all commitment transactions referenced in the ledger
        let mut commitments = Vec::new();
        for commitment_txid in commitment_ledger.tx_ids.iter() {
            let commitment_tx = database::commitment_tx_by_txid(&tx, commitment_txid)
                .expect("Expect to be able to read tx_header from db")
                .expect("Expect commitment transaction to be present in irys_db");

            commitments.push(commitment_tx);
        }

        drop(tx);

        (genesis_block, commitments)
    }

    async fn create_new_genesis_block(
        &self,
        mut genesis_block: IrysBlockHeader,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        // Generate genesis commitments from configuration
        let commitments = get_genesis_commitments(&self.config).await;

        // Calculate initial difficulty based on number of storage modules
        let storage_module_count = (commitments.len() - 1) as u64; // Subtract 1 for stake commitment
        let difficulty = calculate_initial_difficulty(&self.config.consensus, storage_module_count)
            .expect("valid calculated initial difficulty");

        // Create timestamp for genesis block
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let timestamp = now.as_millis();
        genesis_block.diff = difficulty;
        genesis_block.timestamp = timestamp;
        genesis_block.last_diff_timestamp = timestamp;

        // Add commitment transactions to genesis block
        add_genesis_commitments(&mut genesis_block, &self.config).await;

        // Note: commitments are persisted to DB in `persist_genesis_block_and_commitments()` later on

        run_vdf_for_genesis_block(&mut genesis_block, &self.config.consensus.vdf);

        (genesis_block, commitments)
    }

    async fn fetch_genesis_from_trusted_peer(
        &self,
    ) -> (IrysBlockHeader, Vec<CommitmentTransaction>) {
        // Get trusted peer from config
        let trusted_peer = &self
            .config
            .node_config
            .trusted_peers
            .first()
            .expect("expected at least one trusted peer in config")
            .api;

        info!("Fetching genesis block from trusted peer: {}", trusted_peer);

        // Create HTTP client and fetch genesis block
        let awc_client = awc::Client::new();
        let genesis_block = fetch_genesis_block(trusted_peer, &awc_client)
            .await
            .expect("expected genesis block from http api");

        // Fetch associated commitment transactions
        let commitments = fetch_genesis_commitments(trusted_peer, &genesis_block)
            .await
            .expect("Must be able to read genesis commitment tx from trusted peer");

        (genesis_block, commitments)
    }

    /// Persists the genesis block and its associated commitment transactions to the database
    ///
    /// This function is called only during initial blockchain setup
    ///
    /// # Arguments
    /// * `genesis_block` - The genesis block header to persist
    /// * `genesis_commitments` - The commitment transactions associated with the genesis block
    ///
    /// # Returns
    /// * `eyre::Result<()>` - Success or error result of the database operations
    fn persist_genesis_block_and_commitments(
        &self,
        genesis_block: &IrysBlockHeader,
        genesis_commitments: &[CommitmentTransaction],
        irys_db: &DatabaseProvider,
        block_index: &mut BlockIndex,
    ) -> eyre::Result<()> {
        info!("Initializing database with genesis block and commitments");

        // Open a database transaction
        let write_tx = irys_db.tx_mut()?;

        // Insert the genesis block header
        database::insert_block_header(&write_tx, genesis_block)?;

        // Insert all commitment transactions
        for commitment_tx in genesis_commitments {
            debug!("Persisting genesis commitment: {}", commitment_tx.id);
            database::insert_commitment_tx(&write_tx, commitment_tx)?;
        }

        // Commit the database transaction
        write_tx.inner.commit()?;

        block_index.push_block(
            genesis_block,
            &Vec::new(), // Assuming no data transactions in genesis block
            self.config.consensus.chunk_size,
        )?;

        info!("Genesis block and commitments successfully persisted");
        Ok(())
    }

    /// Initializes the node (genesis or non-genesis)
    pub async fn start(self) -> eyre::Result<IrysNodeCtx> {
        // Determine node startup mode
        let config = &self.config;
        let node_mode = &config.node_config.mode;
        // Start with base genesis and update fields
        let (chain_spec, genesis_block) = IrysChainSpecBuilder::from_config(&self.config).build();

        // In all startup modes, irys_db and block_index are prerequisites
        let irys_db = init_irys_db(config).expect("could not open irys db");
        let mut block_index = BlockIndex::new(&config.node_config)
            .await
            .expect("initializing a new block index should be doable");

        // Gets or creates the genesis block and commitments regardless of node mode
        let (genesis_block, genesis_commitments) = self
            .get_or_create_genesis_info(node_mode, genesis_block, &irys_db, &block_index)
            .await;

        // Persist the genesis block to the block_index and db if it's not there already
        if block_index.num_blocks() == 0 {
            self.persist_genesis_block_and_commitments(
                &genesis_block,
                &genesis_commitments,
                &irys_db,
                &mut block_index,
            )?;
        }

        // all async tasks will be run on a new tokio runtime
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let task_manager = TaskManager::new(tokio_runtime.handle().clone());

        // Common node startup logic
        // There are a lot of cross dependencies between reth and irys components, the channels mediate the comms
        let (reth_shutdown_sender, reth_shutdown_receiver) = tokio::sync::mpsc::channel::<()>(1);
        let (main_actor_thread_shutdown_tx, main_actor_thread_shutdown_rx) =
            tokio::sync::mpsc::channel::<()>(1);
        let (vdf_shutdown_sender, vdf_shutdown_receiver) = mpsc::channel(1);
        let (reth_handle_sender, reth_handle_receiver) = oneshot::channel::<RethNode>();
        let (irys_node_ctx_tx, irys_node_ctx_rx) = oneshot::channel::<IrysNodeCtx>();
        let (service_set_tx, service_set_rx) = tokio::sync::oneshot::channel();
        let (shadow_tx_store, _shadow_tx_notification_stream) =
            ShadowTxStore::new_with_notifications();

        let irys_provider = reth_provider::create_provider();

        // read the latest block info
        let (latest_block_height, latest_block) = read_latest_block_data(&block_index, &irys_db);

        // vdf gets started here...
        // init the services
        let actor_main_thread_handle = Self::init_services_thread(
            self.config.clone(),
            Arc::clone(&latest_block),
            reth_shutdown_sender,
            main_actor_thread_shutdown_rx,
            vdf_shutdown_sender,
            vdf_shutdown_receiver,
            reth_handle_receiver,
            service_set_tx,
            irys_node_ctx_tx,
            &irys_provider,
            task_manager.executor(),
            self.http_listener,
            irys_db,
            block_index,
            self.gossip_listener,
            shadow_tx_store.clone(),
            tokio_runtime.handle().clone(),
        )?;

        // start reth
        let reth_thread = Self::init_reth_thread(
            self.config.clone(),
            reth_shutdown_receiver,
            main_actor_thread_shutdown_tx,
            shadow_tx_store,
            reth_handle_sender,
            actor_main_thread_handle,
            irys_provider.clone(),
            chain_spec.clone(),
            latest_block_height,
            task_manager,
            tokio_runtime,
            service_set_rx,
        )?;

        let mut ctx = irys_node_ctx_rx.await?;
        ctx.reth_thread_handle = Some(reth_thread.into());
        let node_config = &ctx.config.node_config;

        // Log startup information
        info!(
            "Started node! ({:?})\nMining address: {}\nReth Peer ID: {}\nHTTP: {}:{},\nGossip: {}:{}\nReth peering: {}",
            &node_mode,
            &ctx.config.node_config.miner_address().to_base58(),
            ctx.reth_handle.network.peer_id(),
            &node_config.http.bind_ip,
            &node_config.http.bind_port,
            &node_config.gossip.bind_ip,
            &node_config.gossip.bind_port,
            &node_config.reth_peer_info.peering_tcp_addr
        );

        let latest_known_block_height = ctx.block_index_guard.read().latest_height();
        // This is going to resolve instantly for a genesis node with 0 blocks,
        //  going to wait for sync otherwise.
        irys_p2p::sync_chain(
            ctx.sync_state.clone(),
            irys_api_client::IrysApiClient::new(),
            &ctx.peer_list,
            latest_known_block_height as usize,
            &ctx.config,
        )
        .await?;

        // Call stake_and_pledge after mempool service is initialized
        if ctx.config.node_config.stake_pledge_drives {
            const MAX_WAIT_TIME: Duration = Duration::from_secs(10);
            let mut validation_tracker = BlockValidationTracker::new(
                ctx.block_tree_guard.clone(),
                ctx.service_senders.clone(),
                MAX_WAIT_TIME,
            );
            // wait for any pending blocks to finish validating
            let latest_hash = validation_tracker.wait_for_validation().await?;

            {
                let btrg = ctx.block_tree_guard.read();
                debug!(
                    "Checking stakes & pledges at height {}, latest hash: {}",
                    btrg.get_canonical_chain().0.last().unwrap().height,
                    &latest_hash
                );
            };
            // TODO: add code to proactively grab the latest head block from peers
            // this only really affects tests, as in a network deployment other nodes will be continuously mining & gossiping, which will trigger a sync to the network head
            stake_and_pledge(
                &ctx.config,
                ctx.block_tree_guard.clone(),
                ctx.storage_modules_guard.clone(),
                latest_hash,
                ctx.mempool_pledge_provider.clone(),
            )
            .await?;
        }

        Ok(ctx)
    }

    fn init_services_thread(
        config: Config,
        latest_block: Arc<IrysBlockHeader>,
        reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
        mut main_actor_thread_shutdown_rx: tokio::sync::mpsc::Receiver<()>,
        vdf_shutdown_sender: mpsc::Sender<()>,
        vdf_shutdown_receiver: mpsc::Receiver<()>,
        reth_handle_receiver: oneshot::Receiver<RethNode>,
        service_set_sender: oneshot::Sender<ServiceSet>,
        irys_node_ctx_tx: oneshot::Sender<IrysNodeCtx>,
        irys_provider: &Arc<RwLock<Option<IrysRethProviderInner>>>,
        task_exec: TaskExecutor,
        http_listener: TcpListener,
        irys_db: DatabaseProvider,
        block_index: BlockIndex,
        gossip_listener: TcpListener,
        shadow_tx_store: ShadowTxStore,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<JoinHandle<()>, eyre::Error> {
        let span = Span::current();
        let actor_main_thread_handle = std::thread::Builder::new()
            .name("actor-main-thread".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn({
                let irys_provider = Arc::clone(irys_provider);
                move || {
                    System::new().block_on(async move {
                        let block_index = Arc::new(RwLock::new(block_index));
                        let block_index_service_actor = Self::init_block_index_service(&config, &block_index);

                        // start the rest of the services
                        let (irys_node, actix_server, vdf_thread,  gossip_service_handle, service_set) = Self::init_services(
                                &config,
                                reth_shutdown_sender,
                                vdf_shutdown_receiver,
                                reth_handle_receiver,
                                block_index,
                                latest_block,
                                irys_provider.clone(),
                                block_index_service_actor,
                                &task_exec,
                                http_listener,
                                irys_db,
                                gossip_listener,
                                shadow_tx_store,
                                runtime_handle,
                            )
                            .instrument(Span::current())
                            .await
                            .expect("initializing services should not fail");
                        service_set_sender.send(service_set).expect("ServiceSet must be sent");
                        irys_node_ctx_tx
                            .send(irys_node)
                            .expect("irys node ctx sender should not be dropped. Is the reth node thread down?");

                        // await on actix web server
                        let server_handle = actix_server.handle();

                        let server_stop_handle = actix_rt::spawn(async move {
                            let _ = main_actor_thread_shutdown_rx.recv().await;
                            info!("Main actor thread received shutdown signal");

                            debug!("Stopping API server");
                            server_handle.stop(true).await;
                            info!("API server stopped");
                        });

                        actix_server.await.unwrap();
                        server_stop_handle.await.unwrap();

                        match gossip_service_handle.stop().await {
                            Ok(()) => info!("Gossip service stopped"),
                            Err(e) => warn!("Gossip service is already stopped: {:?}", e),
                        }

                        // Send shutdown signal
                        vdf_shutdown_sender.send(()).await.unwrap();

                        debug!("Waiting for VDF thread to finish");
                        // Wait for vdf thread to finish & save steps
                        vdf_thread.join().unwrap();

                        debug!("VDF thread finished");
                    }.instrument(span.clone()))
                }
            })?;
        Ok(actor_main_thread_handle)
    }

    fn init_reth_thread(
        config: Config,
        reth_shutdown_receiver: tokio::sync::mpsc::Receiver<()>,
        main_actor_thread_shutdown_tx: tokio::sync::mpsc::Sender<()>,
        shadow_tx_store: ShadowTxStore,
        reth_handle_sender: oneshot::Sender<RethNode>,
        actor_main_thread_handle: JoinHandle<()>,
        irys_provider: IrysRethProvider,
        reth_chainspec: ChainSpec,
        latest_block_height: u64,
        mut task_manager: TaskManager,
        tokio_runtime: Runtime,
        service_set: oneshot::Receiver<ServiceSet>,
    ) -> eyre::Result<JoinHandle<()>> {
        let span = Span::current();
        let span2 = span.clone();

        let reth_thread_handler = std::thread::Builder::new()
            .name("reth-thread".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let exec = task_manager.executor();
                let _span = span.enter();
                let run_reth_until_ctrl_c_or_signal = async || {
                    let node_handle = start_reth_node(
                        exec,
                        reth_chainspec,
                        config,
                        reth_handle_sender,
                        latest_block_height,
                        shadow_tx_store,
                    )
                    .await
                    .expect("to be able to start the reth node");
                    let service_set = service_set.await.expect("Service Set must be awaited");

                    let mut service_set = std::pin::pin!(service_set);
                    let mut task_manager_pinned = std::pin::pin!(&mut task_manager);
                    let reth_node = std::pin::pin!(node_handle.node_exit_future.instrument(span2));

                    let future = async {
                        tokio::select! {
                            _ = &mut service_set => {
                            },
                            res = &mut task_manager_pinned => {
                                tracing::warn!(?res)
                            }
                            _ = reth_node => {}
                        }
                        Ok(())
                    };

                    let _res = run_until_ctrl_c_or_channel_message(future, reth_shutdown_receiver)
                        .await
                        .inspect_err(|e| error!("Reth thread error: {:?}", &e));

                    debug!("Sending shutdown signal to the main actor thread");
                    let _ = main_actor_thread_shutdown_tx.try_send(());

                    debug!("Waiting for the main actor thread to finish");

                    actor_main_thread_handle
                        .join()
                        .expect("to successfully join the actor thread handle");
                    service_set.graceful_shutdown().await;
                    debug!(
                        "Shutting down the rest of the reth jobs in case there are unfinished ones"
                    );
                    task_manager.graceful_shutdown();
                    node_handle.node
                };

                let reth_node =
                    tokio_runtime.block_on(run_reth_until_ctrl_c_or_signal().in_current_span());

                reth_node.provider.database.db.close();
                reth_provider::cleanup_provider(&irys_provider);
                info!("Reth thread finished");
            })?;

        Ok(reth_thread_handler)
    }

    async fn init_services(
        config: &Config,
        reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
        vdf_shutdown_receiver: tokio::sync::mpsc::Receiver<()>,
        reth_handle_receiver: oneshot::Receiver<RethNode>,
        block_index: Arc<RwLock<BlockIndex>>,
        latest_block: Arc<IrysBlockHeader>,
        irys_provider: IrysRethProvider,
        block_index_service_actor: Addr<BlockIndexService>,
        task_exec: &TaskExecutor,
        http_listener: TcpListener,
        irys_db: DatabaseProvider,
        gossip_listener: TcpListener,
        shadow_tx_store: ShadowTxStore,
        runtime_handle: tokio::runtime::Handle,
    ) -> eyre::Result<(
        IrysNodeCtx,
        Server,
        JoinHandle<()>,
        ServiceHandleWithShutdownSignal,
        ServiceSet,
    )> {
        // initialize the databases
        let (reth_node, reth_db) = init_reth_db(reth_handle_receiver).await?;
        debug!("Reth DB initialized");
        let reth_node_adapter =
            IrysRethNodeAdapter::new(reth_node.clone().into(), shadow_tx_store.clone()).await?;

        // start service senders/receivers
        let (service_senders, receivers) = ServiceSenders::new();

        // start reth service
        let (reth_service_actor, reth_arbiter) =
            init_reth_service(&irys_db, reth_node_adapter.clone(), service_senders.clone());
        debug!("Reth Service Actor initialized");
        // Get the correct Reth peer info
        let reth_peering = reth_service_actor.send(GetPeeringInfoMessage {}).await??;

        // overwrite config as we now have reth peering information
        // TODO: Consider if starting the reth service should happen outside of init_services() instead of overwriting config here
        let mut node_config = config.node_config.clone();
        node_config.reth_peer_info = reth_peering;
        let config = Config::new(node_config);

        let chunk_cache_handle = ChunkCacheService::spawn_service(
            irys_db.clone(),
            receivers.chunk_cache,
            config.clone(),
            runtime_handle.clone(),
        );
        debug!("Chunk cache initialized");

        let block_index_guard = block_index_service_actor
            .send(GetBlockIndexGuardMessage)
            .await?;

        // start the broadcast mining service
        let span = Span::current();
        let (broadcast_mining_actor, broadcast_arbiter) = init_broadcaster_service(span.clone());

        // start the epoch service
        let replay_data =
            EpochReplayData::query_replay_data(&irys_db, &block_index_guard, &config).await?;
        // let (genesis_block, commitments, epoch_block_data) = (
        //     &replay_data.genesis_block_header,
        //     &replay_data.genesis_commitments,
        //     &replay_data.epoch_blocks,
        // );

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone())?;

        let p2p_service = P2PService::new(
            config.node_config.miner_address(),
            receivers.gossip_broadcast,
        );
        let sync_state = p2p_service.sync_state.clone();

        // start the block tree service
        let block_tree_handle = BlockTreeService::spawn_service(
            receivers.block_tree,
            irys_db.clone(),
            block_index_guard.clone(),
            &replay_data,
            &storage_submodules_config,
            &config,
            &service_senders,
            reth_service_actor.clone(),
            runtime_handle.clone(),
        );

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let block_tree_sender = service_senders.block_tree.clone();
        let _ = block_tree_sender.send(BlockTreeServiceMessage::GetBlockTreeReadGuard {
            response: oneshot_tx,
        });
        let block_tree_guard = oneshot_rx
            .await
            .expect("to receive BlockTreeReadGuard response from GetBlockTreeReadGuard Message");

        let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
        let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();

        let storage_modules = Self::init_storage_modules(&config, storage_module_infos)?;
        let storage_modules_guard = StorageModulesReadGuard::new(storage_modules.clone());

        // Spawn peer list service
        let (peer_list_service, peer_list_arbiter) = init_peer_list_service(
            &irys_db,
            &config,
            reth_service_actor.clone(),
            receivers.peer_network,
            service_senders.peer_network.clone(),
        );
        let peer_list_guard = peer_list_service
            .send(GetPeerListGuard)
            .await?
            .expect("to get peer list guard");

        let execution_payload_cache =
            ExecutionPayloadCache::new(peer_list_guard.clone(), reth_node_adapter.clone().into());

        // Spawn mempool service
        let mempool_handle = MempoolService::spawn_service(
            irys_db.clone(),
            reth_node_adapter.clone(),
            storage_modules_guard.clone(),
            &block_tree_guard,
            receivers.mempool,
            &config,
            &service_senders,
            runtime_handle.clone(),
        )?;
        let mempool_facade = MempoolServiceFacadeImpl::from(&service_senders);

        // Get the mempool state to create the pledge provider
        let (tx, rx) = oneshot::channel();
        service_senders
            .mempool
            .send(irys_actors::mempool_service::MempoolServiceMessage::GetState(tx))
            .map_err(|_| eyre::eyre!("Failed to send GetState message to mempool service"))?;

        let mempool_state = rx
            .await
            .map_err(|_| eyre::eyre!("Failed to receive mempool state from mempool service"))?;

        // Create the MempoolPledgeProvider
        let mempool_pledge_provider =
            Arc::new(irys_actors::mempool_service::MempoolPledgeProvider::new(
                mempool_state,
                block_tree_guard.clone(),
            ));

        // spawn the chunk migration service
        Self::init_chunk_migration_service(
            &config,
            block_index.clone(),
            &irys_db,
            &service_senders,
            &storage_modules_guard,
        );

        // Spawn VDF service
        let vdf_state = Arc::new(RwLock::new(irys_vdf::state::create_state(
            block_index.clone(),
            irys_db.clone(),
            service_senders.vdf_mining.clone(),
            &config,
        )));
        let vdf_state_readonly = VdfStateReadonly::new(Arc::clone(&vdf_state));

        // Spawn the validation service
        let (validation_handle, validation_enabled) = ValidationService::spawn_service(
            block_index_guard.clone(),
            block_tree_guard.clone(),
            vdf_state_readonly.clone(),
            &config,
            &service_senders,
            reth_node_adapter.clone(),
            irys_db.clone(),
            execution_payload_cache.clone(),
            receivers.validation_service,
            runtime_handle.clone(),
        );

        // create the block reward curve
        let reward_curve = irys_reward_curve::HalvingCurve {
            inflation_cap: config.consensus.block_reward_config.inflation_cap,
            half_life_secs: config.consensus.block_reward_config.half_life_secs.into(),
        };
        let reward_curve = Arc::new(reward_curve);

        // spawn block discovery
        let block_discovery_handle = Self::init_block_discovery_service(
            &config,
            &irys_db,
            &service_senders,
            &block_index_guard,
            &block_tree_guard,
            &vdf_state_readonly,
            Arc::clone(&reward_curve),
            receivers.block_discovery,
            runtime_handle.clone(),
        );

        let block_discovery_facade =
            BlockDiscoveryFacadeImpl::new(service_senders.block_discovery.clone());

        let block_status_provider =
            BlockStatusProvider::new(block_index_guard.clone(), block_tree_guard.clone());

        let (p2p_service_handle, block_pool) = p2p_service.run(
            mempool_facade,
            block_discovery_facade.clone(),
            irys_api_client::IrysApiClient::new(),
            task_exec,
            peer_list_guard.clone(),
            irys_db.clone(),
            gossip_listener,
            block_status_provider.clone(),
            execution_payload_cache,
            vdf_state_readonly.clone(),
            config.clone(),
            service_senders.clone(),
        )?;

        // repair any missing payloads before triggering an FCU
        block_pool
            .repair_missing_payloads_if_any(Some(reth_service_actor.clone()))
            .await?;

        // update reth service about the latest block data it must use
        reth_service_actor
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Evm(latest_block.evm_block_hash),
                confirmed_hash: Some(BlockHashType::Evm(latest_block.evm_block_hash)),
                finalized_hash: None,
            })
            .await??;
        debug!("Reth Service Actor updated about fork choice");

        // set up the price oracle
        let price_oracle = Self::init_price_oracle(&config);

        // set up the block producer
        let (block_producer_inner, block_producer_handle) = Self::init_block_producer(
            &config,
            Arc::clone(&reward_curve),
            &irys_db,
            &service_senders,
            &block_tree_guard,
            &vdf_state_readonly,
            block_discovery_facade.clone(),
            broadcast_mining_actor.clone(),
            price_oracle,
            reth_node_adapter.clone(),
            reth_service_actor.clone(),
            receivers.block_producer,
            reth_node.provider.clone(),
            shadow_tx_store.clone(),
            runtime_handle.clone(),
        );

        let (global_step_number, last_step_hash) =
            vdf_state_readonly.read().get_last_step_and_seed();
        let initial_hash = last_step_hash.0;

        // set up packing actor
        let (atomic_global_step_number, packing_actor_addr, packing_controller_handles) =
            Self::init_packing_actor(
                &config,
                global_step_number,
                &storage_modules_guard,
                runtime_handle.clone(),
            );

        // set up storage modules
        let (part_actors, part_arbiters) = Self::init_partition_mining_actor(
            &config,
            &storage_modules_guard,
            &vdf_state_readonly,
            &service_senders,
            &atomic_global_step_number,
            &packing_actor_addr,
            latest_block.diff,
        );

        // set up the vdf thread
        let vdf_thread_handler = Self::init_vdf_thread(
            &config,
            vdf_shutdown_receiver,
            receivers.vdf_fast_forward,
            receivers.vdf_mining,
            latest_block,
            initial_hash,
            global_step_number,
            broadcast_mining_actor,
            vdf_state,
            atomic_global_step_number,
            block_status_provider,
        );

        // set up chunk provider
        let chunk_provider = Self::init_chunk_provider(&config, storage_modules_guard.clone());

        // set up IrysNodeCtx
        let irys_node_ctx = IrysNodeCtx {
            actor_addresses: ActorAddresses {
                partitions: part_actors,
                packing: packing_actor_addr,
                block_index: block_index_service_actor,
                reth: reth_service_actor,
            },
            reward_curve,
            reth_handle: reth_node.clone(),
            reth_db,
            db: irys_db.clone(),
            chunk_provider: chunk_provider.clone(),
            block_index_guard: block_index_guard.clone(),
            vdf_steps_guard: vdf_state_readonly,
            service_senders: service_senders.clone(),
            reth_shutdown_sender,
            reth_thread_handle: None,
            block_tree_guard: block_tree_guard.clone(),
            config: config.clone(),
            stop_guard: StopGuard::new(),
            peer_list: peer_list_guard.clone(),
            sync_state: sync_state.clone(),
            shadow_tx_store,
            reth_node_adapter,
            block_producer_inner,
            block_pool,
            validation_enabled,
            storage_modules_guard,
            mempool_pledge_provider: mempool_pledge_provider.clone(),
        };

        // Spawn the StorageModuleService to manage the life-cycle of storage modules
        // This service:
        // - Monitors partition assignments from the network
        // - Initializes storage modules when they receive partition assignments
        // - Handles the dynamic addition/removal of storage modules
        // - Coordinates with the epoch service for runtime updates
        debug!("Starting StorageModuleService");
        let storage_module_handle = StorageModuleService::spawn_service(
            receivers.storage_modules,
            storage_modules.clone(),
            &irys_node_ctx.actor_addresses,
            &config,
            runtime_handle.clone(),
        );

        let data_sync_handle = DataSyncService::spawn_service(
            receivers.data_sync,
            block_tree_guard.clone(),
            storage_modules.clone(),
            peer_list_guard.clone(),
            &config,
            runtime_handle.clone(),
        );

        let mut services = Vec::new();
        {
            // Services are shut down in FIFO order (first added = first to shut down)

            // 1. Mining operations
            services.push(ArbiterEnum::ActixArbiter {
                arbiter: ArbiterHandle::new(broadcast_arbiter, "broadcast_arbiter".to_string()),
            });
            services.extend(
                part_arbiters
                    .into_iter()
                    .map(|x| ArbiterEnum::ActixArbiter {
                        arbiter: ArbiterHandle::new(x, "partition_arbiter".to_string()),
                    }),
            );
            // Add packing controllers to services
            services.extend(
                packing_controller_handles
                    .into_iter()
                    .map(ArbiterEnum::TokioService),
            );

            // 2. Block production flow
            services.push(ArbiterEnum::TokioService(block_producer_handle));
            services.push(ArbiterEnum::TokioService(block_discovery_handle));

            // 3. Validation
            services.push(ArbiterEnum::TokioService(validation_handle));

            // 4. Storage operations
            services.push(ArbiterEnum::TokioService(chunk_cache_handle));
            services.push(ArbiterEnum::TokioService(storage_module_handle));
            services.push(ArbiterEnum::TokioService(data_sync_handle));

            // 5. Chain management
            services.push(ArbiterEnum::TokioService(block_tree_handle));

            // 6. State management
            services.push(ArbiterEnum::TokioService(mempool_handle));

            // 7. Core infrastructure (shutdown last)
            services.push(ArbiterEnum::ActixArbiter {
                arbiter: ArbiterHandle::new(peer_list_arbiter, "peer_list_arbiter".to_string()),
            });
            services.push(ArbiterEnum::ActixArbiter {
                arbiter: ArbiterHandle::new(reth_arbiter, "reth_arbiter".to_string()),
            });
        }

        let server = run_server(
            ApiState {
                mempool_service: service_senders.mempool.clone(),
                chunk_provider: chunk_provider.clone(),
                peer_list: peer_list_guard,
                db: irys_db,
                reth_provider: reth_node.clone(),
                block_tree: block_tree_guard.clone(),
                block_index: block_index_guard.clone(),
                config: config.clone(),
                reth_http_url: reth_node
                    .rpc_server_handle()
                    .http_url()
                    .expect("Missing reth rpc url!"),
                sync_state,
                mempool_pledge_provider: mempool_pledge_provider.clone(),
            },
            http_listener,
        );

        // this OnceLock is due to the cyclic chain between Reth & the Irys node, where the IrysRethProvider requires both
        // this is "safe", as the OnceLock is always set before this start function returns
        let mut w = irys_provider
            .write()
            .map_err(|_| eyre::eyre!("lock poisoned"))?;
        *w = Some(IrysRethProviderInner { chunk_provider });

        Ok((
            irys_node_ctx,
            server,
            vdf_thread_handler,
            p2p_service_handle,
            ServiceSet::new(services),
        ))
    }

    fn init_chunk_provider(
        config: &Config,
        storage_modules_guard: StorageModulesReadGuard,
    ) -> Arc<ChunkProvider> {
        let chunk_provider = ChunkProvider::new(config.clone(), storage_modules_guard);

        Arc::new(chunk_provider)
    }

    #[expect(clippy::path_ends_with_ext, reason = "Core pinning logic")]
    fn init_vdf_thread(
        config: &Config,
        vdf_shutdown_receiver: mpsc::Receiver<()>,
        vdf_fast_forward_receiver: mpsc::UnboundedReceiver<VdfStep>,
        vdf_mining_state_rx: mpsc::Receiver<bool>,
        latest_block: Arc<IrysBlockHeader>,
        initial_hash: H256,
        global_step_number: u64,
        broadcast_mining_actor: actix::Addr<BroadcastMiningService>,
        vdf_state: AtomicVdfState,
        atomic_global_step_number: Arc<AtomicU64>,
        block_status_provider: BlockStatusProvider,
    ) -> JoinHandle<()> {
        let next_canonical_vdf_seed = latest_block.vdf_limiter_info.next_seed;
        // FIXME: this should be controlled via a config parameter rather than relying on test-only artifact generation
        // we can't use `cfg!(test)` to detect integration tests, so we check that the path is of form `(...)/.tmp/<random folder>`
        let is_test_based_on_base_dir = config
            .node_config
            .base_directory
            .parent()
            .is_some_and(|p| p.ends_with(".tmp"));
        let is_test_based_on_cfg_flag = cfg!(test);
        if is_test_based_on_cfg_flag && !is_test_based_on_base_dir {
            error!("VDF core pinning: cfg!(test) is true but the base_dir .tmp check is false - please make sure you are using a temporary directory for testing")
        }
        let span = Span::current();

        let vdf_thread_handler = std::thread::spawn({
            let vdf_config = config.consensus.vdf.clone();

            move || {
                let _span = span.enter();

                // Setup core affinity in prod only (perf gain shouldn't matter for tests, and we don't want pinning overlap)
                if is_test_based_on_base_dir || is_test_based_on_cfg_flag {
                    info!("Disabling VDF core pinning")
                } else {
                    let core_ids = core_affinity::get_core_ids().expect("Failed to get core IDs");

                    for core in core_ids {
                        let success = core_affinity::set_for_current(core);
                        if success {
                            info!("VDF thread pinned to core {:?}", core);
                            break;
                        }
                    }
                }

                run_vdf(
                    &vdf_config,
                    global_step_number,
                    initial_hash,
                    next_canonical_vdf_seed,
                    vdf_fast_forward_receiver,
                    vdf_mining_state_rx,
                    vdf_shutdown_receiver,
                    MiningServiceBroadcaster::from(broadcast_mining_actor.clone()),
                    vdf_state.clone(),
                    atomic_global_step_number.clone(),
                    block_status_provider,
                )
            }
        });
        vdf_thread_handler
    }

    fn init_partition_mining_actor(
        config: &Config,
        storage_modules_guard: &StorageModulesReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        service_senders: &ServiceSenders,
        atomic_global_step_number: &Arc<AtomicU64>,
        packing_actor_addr: &actix::Addr<PackingActor>,
        initial_difficulty: U256,
    ) -> (Vec<actix::Addr<PartitionMiningActor>>, Vec<Arbiter>) {
        let mut part_actors = Vec::new();
        let mut arbiters = Vec::new();
        for sm in storage_modules_guard.read().iter() {
            let partition_mining_actor = PartitionMiningActor::new(
                config,
                service_senders.clone(),
                packing_actor_addr.clone().recipient(),
                sm.clone(),
                false, // do not start mining automatically
                vdf_steps_guard.clone(),
                atomic_global_step_number.clone(),
                initial_difficulty,
                Some(Span::current()),
            );
            let part_arbiter = Arbiter::new();
            let partition_mining_actor =
                PartitionMiningActor::start_in_arbiter(&part_arbiter.handle(), |_| {
                    partition_mining_actor
                });
            part_actors.push(partition_mining_actor);
            arbiters.push(part_arbiter);
        }

        // request packing for uninitialized ranges of assigned storage modules
        for sm in storage_modules_guard
            .read()
            .iter()
            .filter(|sm| sm.partition_assignment().is_some())
        {
            let uninitialized = sm.get_intervals(ChunkType::Uninitialized);
            for interval in uninitialized {
                packing_actor_addr.do_send(PackingRequest {
                    storage_module: sm.clone(),
                    chunk_range: PartitionChunkRange(interval),
                });
            }
        }
        (part_actors, arbiters)
    }

    fn init_packing_actor(
        config: &Config,
        global_step_number: u64,
        storage_modules_guard: &StorageModulesReadGuard,
        runtime_handle: tokio::runtime::Handle,
    ) -> (
        Arc<AtomicU64>,
        actix::Addr<PackingActor>,
        Vec<TokioServiceHandle>,
    ) {
        let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));
        let sm_ids = storage_modules_guard.read().iter().map(|s| s.id).collect();
        let packing_config = PackingConfig::new(config);
        let packing_actor = PackingActor::new(sm_ids, packing_config);
        let packing_controller_handles = packing_actor.spawn_packing_controllers(runtime_handle);
        let packing_actor_addr = packing_actor.start();
        (
            atomic_global_step_number,
            packing_actor_addr,
            packing_controller_handles,
        )
    }

    fn init_block_producer(
        config: &Config,
        reward_curve: Arc<HalvingCurve>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_tree_guard: &BlockTreeReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        block_discovery: BlockDiscoveryFacadeImpl,
        broadcast_mining_actor: actix::Addr<BroadcastMiningService>,
        price_oracle: Arc<IrysPriceOracle>,
        reth_node_adapter: IrysRethNodeAdapter,
        reth_service_actor: actix::Addr<RethServiceActor>,
        block_producer_rx: mpsc::UnboundedReceiver<BlockProducerCommand>,
        reth_provider: NodeProvider,
        shadow_tx_store: ShadowTxStore,
        runtime_handle: tokio::runtime::Handle,
    ) -> (Arc<irys_actors::BlockProducerInner>, TokioServiceHandle) {
        let block_producer_inner = Arc::new(irys_actors::BlockProducerInner {
            db: irys_db.clone(),
            config: config.clone(),
            reward_curve,
            mining_broadcaster: broadcast_mining_actor,
            block_discovery,
            vdf_steps_guard: vdf_steps_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            price_oracle,
            service_senders: service_senders.clone(),
            reth_payload_builder: reth_node_adapter.inner.payload_builder_handle.clone(),
            reth_provider,
            shadow_tx_store,
            reth_service: reth_service_actor,
            beacon_engine_handle: reth_node_adapter.inner.beacon_engine_handle.clone(),
        });

        // Spawn the service and get the handle
        let tokio_service_handle = irys_actors::BlockProducerService::spawn_service(
            block_producer_inner.clone(),
            None, // blocks_remaining_for_test
            block_producer_rx,
            runtime_handle,
        );

        (block_producer_inner, tokio_service_handle)
    }

    fn init_price_oracle(config: &Config) -> Arc<IrysPriceOracle> {
        let price_oracle = match config.node_config.oracle {
            OracleConfig::Mock {
                initial_price,
                incremental_change,
                smoothing_interval,
            } => IrysPriceOracle::MockOracle(MockOracle::new(
                initial_price,
                incremental_change,
                smoothing_interval,
            )),
            // note: depending on the oracle, it may require spawning an async background service.
        };

        Arc::new(price_oracle)
    }

    fn init_block_discovery_service(
        config: &Config,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_index_guard: &BlockIndexReadGuard,
        block_tree_guard: &BlockTreeReadGuard,
        vdf_steps_guard: &VdfStateReadonly,
        reward_curve: Arc<HalvingCurve>,
        block_discovery_rx: mpsc::UnboundedReceiver<BlockDiscoveryMessage>,
        runtime_handle: Handle,
    ) -> TokioServiceHandle {
        let block_discovery_inner = BlockDiscoveryServiceInner {
            block_index_guard: block_index_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            db: irys_db.clone(),
            config: config.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            service_senders: service_senders.clone(),
            reward_curve,
        };
        BlockDiscoveryService::spawn_service(
            Arc::new(block_discovery_inner),
            block_discovery_rx,
            runtime_handle,
        )
    }

    fn init_chunk_migration_service(
        config: &Config,
        block_index: Arc<RwLock<BlockIndex>>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        storage_modules_guard: &StorageModulesReadGuard,
    ) {
        let chunk_migration_service = ChunkMigrationService::new(
            block_index,
            config.clone(),
            storage_modules_guard,
            irys_db.clone(),
            service_senders.clone(),
        );
        SystemRegistry::set(chunk_migration_service.start());
    }

    fn init_storage_modules(
        config: &Config,
        storage_module_infos: Vec<StorageModuleInfo>,
    ) -> eyre::Result<Arc<RwLock<Vec<Arc<StorageModule>>>>> {
        let mut storage_modules = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(&info, config)?);
            storage_modules.push(arc_module.clone());
        }

        Ok(Arc::new(RwLock::new(storage_modules)))
    }

    fn init_block_index_service(
        config: &Config,
        block_index: &Arc<RwLock<BlockIndex>>,
    ) -> actix::Addr<BlockIndexService> {
        let block_index_service = BlockIndexService::new(block_index.clone(), &config.consensus);
        let block_index_service_actor = block_index_service.start();
        SystemRegistry::set(block_index_service_actor.clone());
        block_index_service_actor
    }
}

fn read_latest_block_data(
    block_index: &BlockIndex,
    irys_db: &DatabaseProvider,
) -> (u64, Arc<IrysBlockHeader>) {
    let latest_block_index = block_index
        .get_latest_item()
        .cloned()
        .expect("the block index must have at least one entry");
    let latest_block_height = block_index.latest_height();
    let latest_block = Arc::new(
        database::block_header_by_hash(
            &irys_db.tx().unwrap(),
            &latest_block_index.block_hash,
            false,
        )
        .unwrap()
        .unwrap(),
    );
    (latest_block_height, latest_block)
}

fn init_peer_list_service(
    irys_db: &DatabaseProvider,
    config: &Config,
    reth_service_addr: Addr<RethServiceActor>,
    service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
    service_sender: PeerNetworkSender,
) -> (
    Addr<PeerNetworkService<IrysApiClient, RethServiceActor>>,
    Arbiter,
) {
    let peer_list_arbiter = Arbiter::new();
    let peer_list_service = PeerNetworkService::new(
        irys_db.clone(),
        config,
        reth_service_addr,
        service_receiver,
        service_sender,
    );
    let peer_list_service =
        PeerNetworkService::start_in_arbiter(&peer_list_arbiter.handle(), |_| peer_list_service);
    (peer_list_service, peer_list_arbiter)
}

fn init_broadcaster_service(span: Span) -> (actix::Addr<BroadcastMiningService>, Arbiter) {
    let broadcast_arbiter = Arbiter::new();
    let broadcast_mining_actor =
        BroadcastMiningService::start_in_arbiter(&broadcast_arbiter.handle(), |_| {
            BroadcastMiningService {
                span: Some(span),
                ..Default::default()
            }
        });
    SystemRegistry::set(broadcast_mining_actor.clone());
    (broadcast_mining_actor, broadcast_arbiter)
}

fn init_reth_service(
    irys_db: &DatabaseProvider,
    reth_node_adapter: IrysRethNodeAdapter,
    service_senders: ServiceSenders,
) -> (actix::Addr<RethServiceActor>, Arbiter) {
    let reth_service = RethServiceActor::new(
        reth_node_adapter,
        irys_db.clone(),
        service_senders.mempool.clone(),
    );
    let reth_arbiter = Arbiter::new();
    let reth_service_actor =
        RethServiceActor::start_in_arbiter(&reth_arbiter.handle(), |_| reth_service);
    SystemRegistry::set(reth_service_actor.clone());
    (reth_service_actor, reth_arbiter)
}

async fn init_reth_db(
    reth_handle_receiver: oneshot::Receiver<RethNode>,
) -> Result<(RethNodeProvider, irys_database::db::RethDbWrapper), eyre::Error> {
    let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await?));
    let reth_db = reth_node.provider.database.db.clone();
    // TODO: fix this so we can migrate the consensus/irys DB
    // we no longer extend the reth database with our own tables/metadata
    // check_db_version_and_run_migrations_if_needed(&reth_db, irys_db)?;
    Ok((reth_node, reth_db))
}

fn init_irys_db(config: &Config) -> Result<DatabaseProvider, eyre::Error> {
    let irys_db_env =
        open_or_create_irys_consensus_data_db(&config.node_config.irys_consensus_data_dir())?;
    let irys_db = DatabaseProvider(Arc::new(irys_db_env));
    debug!("Irys DB initialized");
    Ok(irys_db)
}

/// This function is used by the node to automatically stake & pledge on startup, if the `node_config.stake_pledge_drives` option is `true`
/// method:
/// 1) check if we have an existing stake - historic (already in an epoch) or pending (ready to be rolled up at the end of the current epoch)
/// 2) if we have no existing stake, submit a stake to the local mempool
/// 3) check all local storage modules for partition assignments against all known partition pledges - historic or pending
/// 4) post enough pledges so that there are enough pledges for all local storage modules
#[instrument(skip_all)]
async fn stake_and_pledge(
    config: &Config,
    block_tree_guard: BlockTreeReadGuard,
    storage_modules_guard: StorageModulesReadGuard,
    latest_block_hash: BlockHash,
    mempool_pledge_provider: Arc<irys_actors::mempool_service::MempoolPledgeProvider>,
) -> eyre::Result<()> {
    debug!("Checking Stake & Pledge status");
    // NOTE: this assumes we're caught up with the chain
    // primarily for the anchor used for the produced txs
    // if we aren't caught up, the txs will be rejected
    let signer = config.irys_signer();
    let address = signer.address();

    let api_uri = config.node_config.api_uri();

    let post_commitment_tx = async |commitment_tx: &CommitmentTransaction| {
        let client = awc::Client::default();
        let url = format!("{}/v1/commitment_tx", api_uri);

        client.post(url).send_json(commitment_tx).await
    };

    // now check the canonical state

    let (is_historically_staked, commitment_snapshot) = {
        let block_tree_guard = block_tree_guard.read();
        let epoch_snapshot = block_tree_guard.canonical_epoch_snapshot();
        let is_historically_staked = epoch_snapshot.is_staked(address);
        let commitment_snapshot = (*block_tree_guard.canonical_commitment_snapshot()).clone();
        (is_historically_staked, commitment_snapshot)
    };

    // check the commitment snapshot (pending commitment txs for the next epoch rollup)
    // to see if we have pending stakes/commitments already
    // if we do, don't submit any more than we need to (i.e if we have added some extra drives etc)
    let pending_commitments = commitment_snapshot.commitments.get(&address);
    let has_pending_stake =
        pending_commitments.is_some_and(|commitments| commitments.stake.is_some());

    // if we have a historic or pending stake, don't send another
    let is_staked = is_historically_staked || has_pending_stake;
    if !is_staked {
        debug!(
            "Local mining address {:?} is not staked, staking...",
            &address
        );

        // post a stake tx
        let stake_tx = CommitmentTransaction::new_stake(&config.consensus, latest_block_hash);
        let stake_tx = signer.sign_commitment(stake_tx)?;

        post_commitment_tx(&stake_tx).await.unwrap();
        debug!("Posted stake tx {:?}", &stake_tx.id);
        stake_tx.id
    } else {
        debug!("Local mining address {:?} is staked", &address);
        // latest_block.previous_block_hash
        latest_block_hash
    };

    // get all SMs without a partition assignment

    let unassigned_modules = {
        let sms = storage_modules_guard.read();
        sms.iter()
            .filter(|&sm| sm.partition_assignment().is_none())
            .count()
    };

    // get the number of pending commitment txs for partitions, if the count is >= the unassigned len, do nothing
    let pending_pledges = pending_commitments.map(|pc| pc.pledges.len()).unwrap_or(0);
    let to_pledge_count = unassigned_modules.saturating_sub(pending_pledges);

    debug!(
        "Found {} SMs without partition assignments ({} pending pledges)",
        &to_pledge_count, &pending_pledges
    );

    for idx in 0..to_pledge_count {
        // post a pledge tx
        let pledge_tx = CommitmentTransaction::new_pledge(
            &config.consensus,
            latest_block_hash,
            mempool_pledge_provider.as_ref(),
            address,
        )
        .await;

        let pledge_tx = signer.sign_commitment(pledge_tx)?;

        post_commitment_tx(&pledge_tx).await.unwrap();
        debug!(
            "Posted pledge tx {}/{} {:?}",
            idx + 1,
            to_pledge_count,
            &pledge_tx.id
        );
    }
    debug!("Stake & Pledge check complete");

    Ok(())
}
