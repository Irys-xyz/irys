use crate::peer_utilities::{fetch_genesis_block, fetch_genesis_commitments};
use crate::vdf::{run_vdf, run_vdf_for_genesis_block};
use actix::{Actor, Addr, Arbiter, System, SystemRegistry};
use actix_web::dev::Server;
use base58::ToBase58;
use irys_actors::{
    block_discovery::BlockDiscoveryActor,
    block_discovery::BlockDiscoveryFacadeImpl,
    block_index_service::{BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerActor,
    block_tree_service::BlockTreeReadGuard,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    broadcast_mining_service::{BroadcastMiningSeed, BroadcastMiningService},
    cache_service::ChunkCacheService,
    chunk_migration_service::ChunkMigrationService,
    ema_service::EmaService,
    epoch_service::{EpochServiceActor, GetPartitionAssignmentsGuardMessage},
    mempool_service::{MempoolService, MempoolServiceFacadeImpl},
    mining::{MiningControl, PartitionMiningActor},
    packing::{PackingActor, PackingConfig, PackingRequest},
    reth_service::{
        BlockHashType, ForkChoiceUpdateMessage, GetPeeringInfoMessage, RethServiceActor,
    },
    services::ServiceSenders,
    validation_service::ValidationService,
    vdf_service::{VdfService, VdfServiceMessage, VdfStepsReadGuard},
};
use irys_actors::{
    ActorAddresses, CommitmentCache, EpochReplayData, GetCommitmentStateGuardMessage,
    StorageModuleService,
};
use irys_api_server::{create_listener, run_server, ApiState};
use irys_config::chain::chainspec::IrysChainSpecBuilder;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::{
    add_genesis_commitments, database, get_genesis_commitments, BlockIndex, SystemLedger,
};
use irys_p2p::{
    P2PService, PeerListService, PeerListServiceFacade, ServiceHandleWithShutdownSignal, SyncState,
};
use irys_price_oracle::{mock_oracle::MockOracle, IrysPriceOracle};
use irys_reth_node_bridge::node::RethNode;
pub use irys_reth_node_bridge::node::{RethNodeAddOns, RethNodeProvider};
use irys_reth_node_bridge::signal::{
    run_to_completion_or_panic, run_until_ctrl_c_or_channel_message,
};
use irys_reward_curve::HalvingCurve;
use irys_storage::StorageModulesReadGuard;
use irys_storage::{
    irys_consensus_data_db::open_or_create_irys_consensus_data_db,
    reth_provider::{IrysRethProvider, IrysRethProviderInner},
    ChunkProvider, ChunkType, StorageModule,
};
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, ArbiterHandle, CloneableJoinHandle,
    CommitmentTransaction, Config, IrysBlockHeader, NodeConfig, NodeMode, OracleConfig,
    PartitionChunkRange, VDFLimiterInfo, H256, U256,
};
use reth::{
    chainspec::ChainSpec,
    tasks::{TaskExecutor, TaskManager},
};
use reth_db::Database as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    net::TcpListener,
    sync::atomic::AtomicU64,
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::sync::oneshot::{self};
use tracing::{debug, error, info, warn, Instrument as _, Span};

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub actor_addresses: ActorAddresses,
    pub arbiters: Arc<RwLock<Vec<ArbiterHandle>>>,
    pub db: DatabaseProvider,
    pub config: Config,
    pub reward_curve: Arc<HalvingCurve>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub block_tree_guard: BlockTreeReadGuard,
    pub vdf_steps_guard: VdfStepsReadGuard,
    pub service_senders: ServiceSenders,
    // Shutdown channels
    pub reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
    // Thread handles spawned by the start function
    pub reth_thread_handle: Option<CloneableJoinHandle<()>>,
    stop_guard: StopGuard,
    pub peer_list: PeerListServiceFacade,
    pub sync_state: SyncState,
}

impl IrysNodeCtx {
    pub fn get_api_state(&self) -> ApiState {
        ApiState {
            mempool_service: self.service_senders.mempool.clone(),
            chunk_provider: self.chunk_provider.clone(),
            ema_service: self.service_senders.ema.clone(),
            peer_list: self.peer_list.clone(),
            db: self.db.clone(),
            config: self.config.clone(),
            reth_provider: self.reth_handle.clone(),
            reth_http_url: self.reth_handle.rpc_server_handle().http_url().unwrap(),
            block_tree: self.block_tree_guard.clone(),
            block_index: self.block_index_guard.clone(),
            sync_state: self.sync_state.clone(),
        }
    }

    pub async fn stop(self) {
        let _ = self.stop_mining();
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

    /// Stop VDF thread mining and send a message to all known partition actors to ignore any received VDF steps
    pub async fn stop_mining(&self) -> eyre::Result<()> {
        // stop VDF thread mining
        if let Err(e) = self.service_senders.vdf_mining.send(false).await {
            tracing::error!("Error sending to vdf_mining_state_sender mspc {:?}", e);
        }
        self.set_partition_mining(false).await
    }
    /// Start VDF thread mining and Send a message to all known partition actors to begin mining when they receive a VDF step
    pub async fn start_mining(&self) -> eyre::Result<()> {
        // start VDF thread mining
        if let Err(e) = self.service_senders.vdf_mining.send(true).await {
            tracing::error!("Error sending to vdf_mining_state_sender mspc {:?}", e);
        }
        self.set_partition_mining(true).await
    }
    // Send a custom control message to all known partition actors to enable/disable partition mining
    pub async fn set_partition_mining(&self, should_mine: bool) -> eyre::Result<()> {
        // Send a custom control message to all known partition actors
        for part in &self.actor_addresses.partitions {
            part.try_send(MiningControl(should_mine))?;
        }
        Ok(())
    }
}

// Shared stop guard that can be cloned
#[derive(Debug)]
struct StopGuard(Arc<AtomicBool>);

impl StopGuard {
    fn new() -> Self {
        StopGuard(Arc::new(AtomicBool::new(false)))
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
        StopGuard(Arc::clone(&self.0))
    }
}

async fn start_reth_node(
    task_executor: TaskExecutor,
    chainspec: ChainSpec,
    config: Config,
    sender: oneshot::Sender<RethNode>,
    irys_provider: IrysRethProvider,
    latest_block: u64,
) -> eyre::Result<()> {
    let random_ports = config.node_config.reth.use_random_ports;
    let node_handle = match irys_reth_node_bridge::node::run_node(
        Arc::new(chainspec.clone()),
        task_executor.clone(),
        config.node_config.clone(),
        irys_provider.clone(),
        latest_block,
        random_ports,
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
                irys_provider.clone(),
                latest_block,
                random_ports,
            )
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

    node_handle.node_exit_future.await
}

/// Builder pattern for configuring and bootstrapping an Irys blockchain node.
pub struct IrysNode {
    pub config: Config,
    pub http_listener: TcpListener,
    pub gossip_listener: TcpListener,
}

impl IrysNode {
    /// Creates a new node builder instance.
    pub async fn new(mut node_config: NodeConfig) -> eyre::Result<Self> {
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
        Ok(IrysNode {
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
    ) -> eyre::Result<(IrysBlockHeader, Vec<CommitmentTransaction>)> {
        info!(miner_address = ?self.config.node_config.miner_address(), "Starting Irys Node: {:?}", node_mode);

        // Check if blockchain data already exists
        let has_existing_data = block_index.num_blocks() > 0;

        if has_existing_data {
            // CASE 1: Load existing genesis block and commitments from database
            return Ok(self.load_existing_genesis(irys_db, block_index).await);
        }

        // CASE 2: No existing data - handle based on node mode
        match node_mode {
            NodeMode::Genesis => {
                // Create a new genesis block for network initialization
                return self.create_new_genesis_block(genesis_block.clone()).await;
            }
            NodeMode::PeerSync => {
                // Fetch genesis data from trusted peer when joining network
                return Ok(self.fetch_genesis_from_trusted_peer().await);
            }
        }
    }

    // Helper methods to flatten the main function
    async fn load_existing_genesis(
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
    ) -> eyre::Result<(IrysBlockHeader, Vec<CommitmentTransaction>)> {
        // Generate genesis commitments from configuration
        let commitments = get_genesis_commitments(&self.config);

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
        add_genesis_commitments(&mut genesis_block, &self.config);

        let last_epoch_hash = &genesis_block.last_epoch_hash;

        genesis_block.vdf_limiter_info = VDFLimiterInfo {
            global_step_number: 1,
            prev_output: *last_epoch_hash,
            ..VDFLimiterInfo::default()
        };

        run_vdf_for_genesis_block(&mut genesis_block, &self.config.consensus.vdf);

        // self.config
        //     .irys_signer()
        //     .sign_block_header(&mut genesis_block)?;

        Ok((genesis_block, commitments))
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
    async fn persist_genesis_block_and_commitments(
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
            &genesis_block,
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
        let irys_db = init_irys_db(&config).expect("could not open irys db");
        let mut block_index = BlockIndex::new(&config.node_config)
            .await
            .expect("initializing a new block index should be doable");

        // Gets or creates the genesis block and commitments regardless of node mode
        let (genesis_block, genesis_commitments) = self
            .get_or_create_genesis_info(node_mode, genesis_block, &irys_db, &block_index)
            .await?;

        // Persist the genesis block to the block_index and db if it's not there already
        if block_index.num_blocks() == 0 {
            self.persist_genesis_block_and_commitments(
                &genesis_block,
                &genesis_commitments,
                &irys_db,
                &mut block_index,
            )
            .await?;
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

        let irys_provider = irys_storage::reth_provider::create_provider();

        // init the services
        let (latest_block_height_tx, latest_block_height_rx) = oneshot::channel::<u64>();

        // vdf gets started here...
        let actor_main_thread_handle = Self::init_services_thread(
            self.config.clone(),
            latest_block_height_tx,
            reth_shutdown_sender,
            main_actor_thread_shutdown_rx,
            vdf_shutdown_sender,
            vdf_shutdown_receiver,
            reth_handle_receiver,
            irys_node_ctx_tx,
            &irys_provider,
            task_manager.executor(),
            self.http_listener,
            irys_db,
            block_index,
            self.gossip_listener,
        )?;

        // await the latest height to be reported
        let latest_height = latest_block_height_rx.await?;

        // start reth
        let reth_thread = Self::init_reth_thread(
            self.config.clone(),
            reth_shutdown_receiver,
            main_actor_thread_shutdown_tx,
            reth_handle_sender,
            actor_main_thread_handle,
            irys_provider.clone(),
            chain_spec.clone(),
            latest_height,
            task_manager,
            tokio_runtime,
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
            ctx.peer_list.clone(),
            node_mode,
            latest_known_block_height as usize,
            ctx.config.node_config.genesis_peer_discovery_timeout_millis,
        )
        .await?;

        Ok(ctx)
    }

    fn init_services_thread(
        config: Config,
        latest_block_height_tx: oneshot::Sender<u64>,
        reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
        mut main_actor_thread_shutdown_rx: tokio::sync::mpsc::Receiver<()>,
        vdf_shutdown_sender: mpsc::Sender<()>,
        vdf_shutdown_receiver: mpsc::Receiver<()>,
        reth_handle_receiver: oneshot::Receiver<RethNode>,
        irys_node_ctx_tx: oneshot::Sender<IrysNodeCtx>,
        irys_provider: &Arc<RwLock<Option<IrysRethProviderInner>>>,
        task_exec: TaskExecutor,
        http_listener: TcpListener,
        irys_db: DatabaseProvider,
        block_index: BlockIndex,
        gossip_listener: TcpListener,
    ) -> Result<JoinHandle<RethNodeProvider>, eyre::Error> {
        let span = Span::current();
        let actor_main_thread_handle = std::thread::Builder::new()
            .name("actor-main-thread".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn({
                let irys_provider = Arc::clone(&irys_provider);
                move || {
                    System::new().block_on(async move {
                        // read the latest block info
                        let (latest_block_height, latest_block) =
                            read_latest_block_data(&block_index, &irys_db).await;
                        latest_block_height_tx
                            .send(latest_block_height)
                            .expect("to be able to send the latest block height");
                        let block_index = Arc::new(RwLock::new(block_index));
                        let block_index_service_actor = Self::init_block_index_service(&config, &block_index);

                        // start the rest of the services
                        let (irys_node, actix_server, vdf_thread, reth_node, gossip_service_handle) = Self::init_services(
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
                                gossip_listener
                            )
                            .instrument(Span::current())
                            .await
                            .expect("initializing services should not fail");

                        let arbiters_guard = irys_node.arbiters.clone();
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
                            Ok(_) => info!("Gossip service stopped"),
                            Err(e) => warn!("Gossip service is already stopped: {:?}", e),
                        }

                        debug!("Stopping actors");
                        let arbiters = arbiters_guard.read().unwrap();
                        for arbiter in arbiters.iter() {
                            arbiter.clone().stop_and_join();
                        }
                        debug!("Actors stopped");

                        // Send shutdown signal
                        vdf_shutdown_sender.send(()).await.unwrap();

                        debug!("Waiting for VDF thread to finish");
                        // Wait for vdf thread to finish & save steps
                        vdf_thread.join().unwrap();

                        debug!("VDF thread finished");
                        reth_node
                    }.instrument(span.clone()))
                }
            })?;
        Ok(actor_main_thread_handle)
    }

    fn init_reth_thread(
        config: Config,
        reth_shutdown_receiver: tokio::sync::mpsc::Receiver<()>,
        main_actor_thread_shutdown_tx: tokio::sync::mpsc::Sender<()>,
        reth_handle_sender: oneshot::Sender<RethNode>,
        actor_main_thread_handle: JoinHandle<RethNodeProvider>,
        irys_provider: IrysRethProvider,
        reth_chainspec: ChainSpec,
        latest_block_height: u64,
        mut task_manager: TaskManager,
        tokio_runtime: Runtime,
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
                    _ = run_to_completion_or_panic(
                        &mut task_manager,
                        // todo we can simplify things if we use `irys_reth_node_bridge::run_node` directly
                        //      Then we can drop the channel
                        run_until_ctrl_c_or_channel_message(
                            start_reth_node(
                                exec,
                                reth_chainspec,
                                config,
                                reth_handle_sender,
                                irys_provider.clone(),
                                latest_block_height,
                            )
                            .instrument(span2),
                            reth_shutdown_receiver,
                        ),
                    )
                    .await
                    .inspect_err(|e| error!("Reth thread error: {:?}", &e));

                    debug!("Sending shutdown signal to the main actor thread");
                    let _ = main_actor_thread_shutdown_tx.try_send(());

                    debug!("Waiting for the main actor thread to finish");
                    let reth_node_handle = actor_main_thread_handle
                        .join()
                        .expect("to successfully join the actor thread handle");

                    reth_node_handle
                };

                let reth_node = tokio_runtime.block_on(run_reth_until_ctrl_c_or_signal());

                debug!("Shutting down the rest of the reth jobs in case there are unfinished ones");
                task_manager.graceful_shutdown();

                reth_node.provider.database.db.close();
                irys_storage::reth_provider::cleanup_provider(&irys_provider);
                info!("Reth thread finished");
            })?;

        return Ok(reth_thread_handler);
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
    ) -> eyre::Result<(
        IrysNodeCtx,
        Server,
        JoinHandle<()>,
        RethNodeProvider,
        ServiceHandleWithShutdownSignal,
    )> {
        // initialize the databases
        let (reth_node, reth_db) = init_reth_db(reth_handle_receiver).await?;
        debug!("Reth DB initialized");

        // start service senders/receivers
        let (service_senders, receivers) = ServiceSenders::new();

        // start reth service
        let (reth_service_actor, reth_arbiter) = init_reth_service(&irys_db, &reth_node);
        debug!("Reth Service Actor initialized");
        // Get the correct Reth peer info
        let reth_peering = reth_service_actor.send(GetPeeringInfoMessage {}).await??;

        // overwrite config as we now have reth peering information
        // TODO: Consider if starting the reth service should happen outside of init_services() instead of overwriting config here
        let mut node_config = config.node_config.clone();
        node_config.reth_peer_info = reth_peering;
        let config = Config::new(node_config);

        // update reth service about the latest block data it must use
        reth_service_actor
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Evm(latest_block.evm_block_hash),
                confirmed_hash: Some(BlockHashType::Evm(latest_block.evm_block_hash)),
                finalized_hash: None,
            })
            .await??;
        debug!("Reth Service Actor updated about fork choice");

        let _handle = ChunkCacheService::spawn_service(
            &task_exec,
            irys_db.clone(),
            receivers.chunk_cache,
            config.clone(),
        );
        debug!("Chunk cache initialized");

        let block_index_guard = block_index_service_actor
            .send(GetBlockIndexGuardMessage)
            .await?;

        // start the broadcast mining service
        let span = Span::current();
        let (broadcast_mining_actor, broadcast_arbiter) = init_broadcaster_service(span.clone());

        // start the epoch service
        let (storage_module_infos, epoch_service_actor) =
            Self::init_epoch_service(&config, &service_senders, &irys_db, &block_index_guard)
                .await?;

        // Retrieve Partition assignment
        let partition_assignments_guard = epoch_service_actor
            .send(GetPartitionAssignmentsGuardMessage)
            .await?;
        let storage_modules = Self::init_storage_modules(&config, storage_module_infos)?;
        let storage_modules_guard = StorageModulesReadGuard::new(storage_modules.clone());

        // Retrieve Commitment State
        let commitment_state_guard = epoch_service_actor
            .send(GetCommitmentStateGuardMessage)
            .await?;

        let p2p_service = P2PService::new(
            config.node_config.miner_address(),
            receivers.gossip_broadcast,
        );
        let sync_state = p2p_service.sync_state.clone();

        // start the block tree service
        let (block_tree_service, block_tree_arbiter) = Self::init_block_tree_service(
            &config,
            &block_index,
            &irys_db,
            &service_senders,
            &block_index_guard,
        );
        let block_tree_guard = block_tree_service.send(GetBlockTreeGuardMessage).await?;

        // Spawn EMA service
        let _handle =
            EmaService::spawn_service(&task_exec, block_tree_guard.clone(), receivers.ema, &config);

        // Spawn the CommitmentCache service
        let _commitcache_handle = CommitmentCache::spawn_service(
            &task_exec,
            receivers.commitments_cache,
            commitment_state_guard.clone(),
        );

        // Spawn peer list service
        let (peer_list_service, peer_list_arbiter) =
            init_peer_list_service(&irys_db, &config, reth_service_actor.clone());

        // Spawn mempool service
        let _mempool_handle = MempoolService::spawn_service(
            &task_exec,
            &irys_db,
            reth_db,
            &storage_modules_guard,
            &block_tree_guard,
            &commitment_state_guard,
            receivers.mempool,
            &config,
            &service_senders,
        );
        let mempool_facade = MempoolServiceFacadeImpl::from(service_senders.mempool.clone());

        // spawn the chunk migration service
        Self::init_chunk_migration_service(
            &config,
            block_index.clone(),
            &irys_db,
            &service_senders,
            &storage_modules_guard,
        );

        // Spawn VDF service
        let _handle = VdfService::spawn_service(
            &task_exec,
            irys_db.clone(),
            block_index_guard.clone(),
            receivers.vdf,
            service_senders.vdf_mining.clone(),
            &config,
        );

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let vdf_service_sender = service_senders.vdf.clone();
        let _ = vdf_service_sender.send(VdfServiceMessage::GetVdfStateMessage {
            response: oneshot_tx,
        });
        let vdf_steps_guard = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard response from GetVdfStateMessage");

        // spawn the validation service
        let validation_arbiter = Self::init_validation_service(
            &config,
            &block_index_guard,
            &partition_assignments_guard,
            &vdf_steps_guard,
            service_senders.vdf_fast_forward.clone(),
            service_senders.vdf.clone(),
        );

        // create the block reward curve
        let reward_curve = irys_reward_curve::HalvingCurve {
            inflation_cap: config.consensus.block_reward_config.inflation_cap,
            half_life_secs: config.consensus.block_reward_config.half_life_secs.into(),
        };
        let reward_curve = Arc::new(reward_curve);

        // spawn block discovery
        let (block_discovery, block_discovery_arbiter) = Self::init_block_discovery_service(
            &config,
            &irys_db,
            &service_senders,
            &epoch_service_actor,
            &block_index_guard,
            partition_assignments_guard,
            &vdf_steps_guard,
            Arc::clone(&reward_curve),
        );
        let block_discovery_facade = BlockDiscoveryFacadeImpl::new(block_discovery.clone());

        let p2p_service_handle = p2p_service.run(
            mempool_facade,
            block_discovery_facade,
            irys_api_client::IrysApiClient::new(),
            task_exec,
            peer_list_service.clone(),
            irys_db.clone(),
            service_senders.vdf_fast_forward.clone(),
            gossip_listener,
            service_senders.vdf.clone(),
        )?;

        // set up the price oracle
        let price_oracle = Self::init_price_oracle(&config);

        // set up the block producer

        let (block_producer_addr, block_producer_arbiter) = Self::init_block_producer(
            &config,
            Arc::clone(&reward_curve),
            &irys_db,
            &reth_node,
            &service_senders,
            &epoch_service_actor,
            &block_tree_guard,
            &vdf_steps_guard,
            block_discovery.clone(),
            price_oracle,
        );

        let (global_step_number, seed) = vdf_steps_guard.read().get_last_step_and_seed();
        let seed = seed
            .map(|x| x.0)
            .unwrap_or(latest_block.vdf_limiter_info.seed);

        // let (global_step_number, seed) = (0, H256::zero());

        // set up packing actor
        let (atomic_global_step_number, packing_actor_addr) = Self::init_packing_actor(
            &config,
            global_step_number,
            &reth_node,
            &storage_modules_guard,
        );

        // set up storage modules
        let (part_actors, part_arbiters) = Self::init_partition_mining_actor(
            &config,
            &storage_modules_guard,
            &vdf_steps_guard,
            &block_producer_addr,
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
            seed,
            global_step_number,
            broadcast_mining_actor,
            service_senders.vdf.clone(),
            atomic_global_step_number,
        );

        // set up chunk provider
        let chunk_provider = Self::init_chunk_provider(&config, storage_modules_guard);

        // set up IrysNodeCtx
        let irys_node_ctx = IrysNodeCtx {
            actor_addresses: ActorAddresses {
                partitions: part_actors,
                block_discovery_addr: block_discovery,
                block_producer: block_producer_addr,
                packing: packing_actor_addr,
                block_index: block_index_service_actor,
                epoch_service: epoch_service_actor,
                reth: reth_service_actor,
            },
            arbiters: Arc::new(RwLock::new(Vec::new())),
            reward_curve,
            reth_handle: reth_node.clone(),
            db: irys_db.clone(),
            chunk_provider: chunk_provider.clone(),
            block_index_guard: block_index_guard.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            service_senders: service_senders.clone(),
            reth_shutdown_sender,
            reth_thread_handle: None,
            block_tree_guard: block_tree_guard.clone(),
            config: config.clone(),
            stop_guard: StopGuard::new(),
            peer_list: peer_list_service.clone(),
            sync_state: sync_state.clone(),
        };

        // Spawn the StorageModuleService to manage the lifecycle of storage modules
        // This service:
        // - Monitors partition assignments from the network
        // - Initializes storage modules when they receive partition assignments
        // - Handles the dynamic addition/removal of storage modules
        // - Coordinates with the epoch service for runtime updates
        debug!("Starting StorageModuleService");
        let _handle = StorageModuleService::spawn_service(
            &task_exec,
            receivers.storage_modules,
            storage_modules,
            &irys_node_ctx.actor_addresses,
            &config,
        );

        let mut arbiters_guard = irys_node_ctx.arbiters.write().unwrap();

        arbiters_guard.push(ArbiterHandle::new(
            block_producer_arbiter,
            "block_producer_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(
            broadcast_arbiter,
            "broadcast_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(
            block_discovery_arbiter,
            "block_discovery_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(
            validation_arbiter,
            "validation_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(
            block_tree_arbiter,
            "block_tree_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(
            peer_list_arbiter,
            "peer_list_arbiter".to_string(),
        ));
        arbiters_guard.push(ArbiterHandle::new(reth_arbiter, "reth_arbiter".to_string()));
        arbiters_guard.extend(
            part_arbiters
                .into_iter()
                .map(|x| ArbiterHandle::new(x, "partition_arbiter".to_string())),
        );
        drop(arbiters_guard);

        let server = run_server(
            ApiState {
                mempool_service: service_senders.mempool.clone(),
                ema_service: service_senders.ema.clone(),
                chunk_provider: chunk_provider.clone(),
                peer_list: peer_list_service,
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
            },
            http_listener,
        )
        .await;

        // this OnceLock is due to the cyclic chain between Reth & the Irys node, where the IrysRethProvider requires both
        // this is "safe", as the OnceLock is always set before this start function returns
        let mut w = irys_provider
            .write()
            .map_err(|_| eyre::eyre!("lock poisoned"))?;
        *w = Some(IrysRethProviderInner {
            chunk_provider: chunk_provider.clone(),
        });

        Ok((
            irys_node_ctx,
            server,
            vdf_thread_handler,
            reth_node,
            p2p_service_handle,
        ))
    }

    fn init_chunk_provider(
        config: &Config,
        storage_modules_guard: StorageModulesReadGuard,
    ) -> Arc<ChunkProvider> {
        let chunk_provider = ChunkProvider::new(config.clone(), storage_modules_guard.clone());
        let chunk_provider = Arc::new(chunk_provider);
        chunk_provider
    }

    fn init_vdf_thread(
        config: &Config,
        vdf_shutdown_receiver: mpsc::Receiver<()>,
        fast_forward_rx: mpsc::Receiver<BroadcastMiningSeed>,
        vdf_mining_state_rx: mpsc::Receiver<bool>,
        latest_block: Arc<IrysBlockHeader>,
        seed: H256,
        global_step_number: u64,
        broadcast_mining_actor: actix::Addr<BroadcastMiningService>,
        vdf_service: tokio::sync::mpsc::UnboundedSender<VdfServiceMessage>,
        atomic_global_step_number: Arc<AtomicU64>,
    ) -> JoinHandle<()> {
        let vdf_reset_seed = latest_block.vdf_limiter_info.seed;
        // FIXME: this should be controlled via a config parameter rather than relying on test-only artifact generation
        // we can't use `cfg!(test)` to detect integration tests, so we check that the path is of form `(...)/.tmp/<random folder>`
        let is_test = config
            .node_config
            .base_directory
            .parent()
            .is_some_and(|p| p.ends_with(".tmp"));
        let span = Span::current();

        let vdf_thread_handler = std::thread::spawn({
            let vdf_config = config.consensus.vdf.clone();

            move || {
                let _span = span.enter();

                if !is_test {
                    // Setup core affinity in prod only (perf gain shouldn't matter for tests, and we don't want pinning overlap)
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
                    seed,
                    vdf_reset_seed,
                    fast_forward_rx,
                    vdf_mining_state_rx,
                    vdf_shutdown_receiver,
                    broadcast_mining_actor.clone(),
                    vdf_service.clone(),
                    atomic_global_step_number.clone(),
                )
            }
        });
        vdf_thread_handler
    }

    fn init_partition_mining_actor(
        config: &Config,
        storage_modules_guard: &StorageModulesReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
        block_producer_addr: &actix::Addr<BlockProducerActor>,
        atomic_global_step_number: &Arc<AtomicU64>,
        packing_actor_addr: &actix::Addr<PackingActor>,
        initial_difficulty: U256,
    ) -> (Vec<actix::Addr<PartitionMiningActor>>, Vec<Arbiter>) {
        let mut part_actors = Vec::new();
        let mut arbiters = Vec::new();
        for sm in storage_modules_guard.read().iter() {
            let partition_mining_actor = PartitionMiningActor::new(
                &config,
                block_producer_addr.clone().recipient(),
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

        // request packing for uninitialized ranges
        for sm in storage_modules_guard.read().iter() {
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
        reth_node: &RethNodeProvider,
        storage_modules_guard: &StorageModulesReadGuard,
    ) -> (Arc<AtomicU64>, actix::Addr<PackingActor>) {
        let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));
        let sm_ids = storage_modules_guard
            .read()
            .iter()
            .map(|s| (*s).id)
            .collect();
        let packing_config = PackingConfig::new(&config);
        let packing_actor_addr = PackingActor::new(
            reth_node.task_executor.clone(),
            sm_ids,
            packing_config.clone(),
        )
        .start();
        (atomic_global_step_number, packing_actor_addr)
    }

    fn init_block_producer(
        config: &Config,
        reward_curve: Arc<HalvingCurve>,
        irys_db: &DatabaseProvider,
        reth_node: &RethNodeProvider,
        service_senders: &ServiceSenders,
        epoch_service_actor: &actix::Addr<EpochServiceActor>,
        block_tree_guard: &BlockTreeReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
        block_discovery: actix::Addr<BlockDiscoveryActor>,
        price_oracle: Arc<IrysPriceOracle>,
    ) -> (actix::Addr<BlockProducerActor>, Arbiter) {
        let block_producer_arbiter = Arbiter::new();
        let block_producer_actor = BlockProducerActor {
            db: irys_db.clone(),
            config: config.clone(),
            reward_curve,
            block_discovery_addr: block_discovery,
            epoch_service: epoch_service_actor.clone(),
            reth_provider: reth_node.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            price_oracle,
            service_senders: service_senders.clone(),
            blocks_remaining_for_test: None,
            span: Span::current(),
        };
        let block_producer_addr =
            BlockProducerActor::start_in_arbiter(&block_producer_arbiter.handle(), |_| {
                block_producer_actor
            });
        (block_producer_addr, block_producer_arbiter)
    }

    fn init_price_oracle(config: &Config) -> Arc<IrysPriceOracle> {
        let price_oracle = match config.node_config.oracle {
            OracleConfig::Mock {
                initial_price,
                percent_change,
                smoothing_interval,
            } => IrysPriceOracle::MockOracle(MockOracle::new(
                initial_price,
                percent_change,
                smoothing_interval,
            )),
            // note: depending on the oracle, it may require spawning an async background service.
        };
        let price_oracle = Arc::new(price_oracle);
        price_oracle
    }

    fn init_block_discovery_service(
        config: &Config,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        epoch_service: &Addr<EpochServiceActor>,
        block_index_guard: &BlockIndexReadGuard,
        partition_assignments_guard: irys_actors::epoch_service::PartitionAssignmentsReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
        reward_curve: Arc<HalvingCurve>,
    ) -> (actix::Addr<BlockDiscoveryActor>, Arbiter) {
        let block_discovery_actor = BlockDiscoveryActor {
            block_index_guard: block_index_guard.clone(),
            partition_assignments_guard: partition_assignments_guard.clone(),
            db: irys_db.clone(),
            config: config.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            service_senders: service_senders.clone(),
            epoch_service: epoch_service.clone(),
            reward_curve,
            span: Span::current(),
        };
        let block_discovery_arbiter = Arbiter::new();
        let block_discovery =
            BlockDiscoveryActor::start_in_arbiter(&block_discovery_arbiter.handle(), |_| {
                block_discovery_actor
            });
        (block_discovery, block_discovery_arbiter)
    }

    fn init_validation_service(
        config: &Config,
        block_index_guard: &BlockIndexReadGuard,
        partition_assignments_guard: &irys_actors::epoch_service::PartitionAssignmentsReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
        vdf_fast_forward_sender: Sender<BroadcastMiningSeed>,
        vdf_service_sender: UnboundedSender<VdfServiceMessage>,
    ) -> Arbiter {
        let validation_service = ValidationService::new(
            block_index_guard.clone(),
            partition_assignments_guard.clone(),
            vdf_steps_guard.clone(),
            config,
            vdf_fast_forward_sender,
            vdf_service_sender,
        );
        let validation_arbiter = Arbiter::new();
        let validation_service =
            ValidationService::start_in_arbiter(&validation_arbiter.handle(), |_| {
                validation_service
            });
        SystemRegistry::set(validation_service);
        validation_arbiter
    }

    fn init_chunk_migration_service(
        config: &Config,
        block_index: Arc<RwLock<BlockIndex>>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        storage_modules_guard: &StorageModulesReadGuard,
    ) {
        let chunk_migration_service = ChunkMigrationService::new(
            block_index.clone(),
            config.clone(),
            storage_modules_guard,
            irys_db.clone(),
            service_senders.clone(),
        );
        SystemRegistry::set(chunk_migration_service.start());
    }

    fn init_block_tree_service(
        config: &Config,
        block_index: &Arc<RwLock<BlockIndex>>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_index_guard: &BlockIndexReadGuard,
    ) -> (actix::Addr<BlockTreeService>, Arbiter) {
        let block_tree_service = BlockTreeService::new(
            irys_db.clone(),
            block_index.clone(),
            &config.node_config.miner_address(),
            block_index_guard.clone(),
            config.consensus.clone(),
            service_senders.clone(),
        );
        let block_tree_arbiter = Arbiter::new();
        let block_tree_service =
            BlockTreeService::start_in_arbiter(&block_tree_arbiter.handle(), |_| {
                block_tree_service
            });
        SystemRegistry::set(block_tree_service.clone());
        (block_tree_service, block_tree_arbiter)
    }

    fn init_storage_modules(
        config: &Config,
        storage_module_infos: Vec<irys_storage::StorageModuleInfo>,
    ) -> eyre::Result<Arc<RwLock<Vec<Arc<StorageModule>>>>> {
        let mut storage_modules = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(&info, &config)?);
            storage_modules.push(arc_module.clone());
        }

        Ok(Arc::new(RwLock::new(storage_modules)))
    }

    async fn init_epoch_service(
        config: &Config,
        service_senders: &ServiceSenders,
        irys_db: &DatabaseProvider,
        block_index_guard: &BlockIndexReadGuard,
    ) -> eyre::Result<(
        Vec<irys_storage::StorageModuleInfo>,
        actix::Addr<EpochServiceActor>,
    )> {
        let (genesis_block, commitments, epoch_replay_data) =
            EpochReplayData::query_replay_data(irys_db, block_index_guard, &config)?;

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone())?;
        let mut epoch_service =
            EpochServiceActor::new(service_senders, &storage_submodules_config, &config);

        let _ = epoch_service.initialize(genesis_block, commitments)?;
        let storage_module_infos = epoch_service.replay_epoch_data(epoch_replay_data)?;
        let epoch_service_actor = epoch_service.start();
        Ok((storage_module_infos, epoch_service_actor))
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

async fn read_latest_block_data(
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
) -> (PeerListServiceFacade, Arbiter) {
    let peer_list_arbiter = Arbiter::new();
    let mut peer_list_service = PeerListService::new(irys_db.clone(), config, reth_service_addr);
    peer_list_service
        .initialize()
        .expect("to initialize peer_list_service");
    let peer_list_service =
        PeerListService::start_in_arbiter(&peer_list_arbiter.handle(), |_| peer_list_service);
    SystemRegistry::set(peer_list_service.clone());
    (peer_list_service.into(), peer_list_arbiter)
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
    reth_node: &RethNodeProvider,
) -> (actix::Addr<RethServiceActor>, Arbiter) {
    let reth_service = RethServiceActor::new(reth_node.clone(), irys_db.clone());
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
    debug!("Irys DB initiailsed");
    Ok(irys_db)
}
