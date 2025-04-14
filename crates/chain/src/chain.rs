use crate::arbiter_handle::{ArbiterHandle, CloneableJoinHandle};
use crate::peer_utilities::{fetch_genesis_block, sync_state_from_peers};
use crate::vdf::run_vdf;
use actix::{Actor, Addr, Arbiter, System, SystemRegistry};
use actix_web::dev::Server;
use irys_actors::{
    block_discovery::BlockDiscoveryActor,
    block_index_service::{BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerActor,
    block_tree_service::BlockTreeReadGuard,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    cache_service::ChunkCacheService,
    chunk_migration_service::ChunkMigrationService,
    ema_service::EmaService,
    epoch_service::{EpochServiceActor, EpochServiceConfig, GetPartitionAssignmentsGuardMessage},
    mempool_service::MempoolService,
    mining::PartitionMiningActor,
    packing::PackingConfig,
    packing::{PackingActor, PackingRequest},
    peer_list_service::{AddPeerMessage, PeerListService},
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    validation_service::ValidationService,
    vdf_service::{GetVdfStateMessage, VdfService},
    ActorAddresses, BlockFinalizedMessage,
};
use irys_api_server::{create_listener, run_server, ApiState};
use irys_config::{IrysNodeConfig, StorageSubmodulesConfig};
use irys_database::{
    add_genesis_commitments, database, get_genesis_commitments, insert_commitment_tx,
    migration::check_db_version_and_run_migrations_if_needed, tables::IrysTables, BlockIndex,
    Initialized,
};
use irys_gossip_service::ServiceHandleWithShutdownSignal;
use irys_price_oracle::{mock_oracle::MockOracle, IrysPriceOracle};

pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};
use irys_storage::{
    irys_consensus_data_db::open_or_create_irys_consensus_data_db,
    reth_provider::{IrysRethProvider, IrysRethProviderInner},
    ChunkProvider, ChunkType, StorageModule,
};

use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, vdf_config::VDFStepsConfig,
    CommitmentTransaction, Config, DifficultyAdjustmentConfig, GossipData, IrysBlockHeader,
    OracleConfig, PartitionChunkRange, PeerAddress, PeerListItem, StorageConfig, H256,
};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c_or_channel_message};
use reth_db::{Database as _, HasName, HasTableType};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::atomic::AtomicU64,
    sync::{mpsc, Arc, RwLock},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tokio::sync::oneshot::{self};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub node_config: Arc<IrysNodeConfig>,
    pub config: Arc<Config>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub block_tree_guard: BlockTreeReadGuard,
    pub vdf_steps_guard: VdfStepsReadGuard,
    pub vdf_config: VDFStepsConfig,
    pub storage_config: StorageConfig,
    pub service_senders: ServiceSenders,
    // Shutdown channels
    pub reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
    // Thread handles spawned by the start function
    pub reth_thread_handle: Option<CloneableJoinHandle<()>>,
    stop_guard: StopGuard,
}

impl IrysNodeCtx {
    pub async fn stop(self) {
        let _ = self.actor_addresses.stop_mining();
        debug!("Sending shutdown signal to reth thread");
        // Shutting down reth node will propagate to the main actor thread eventually
        let _ = self.reth_shutdown_sender.send(()).await;
        let _ = self.reth_thread_handle.unwrap().join();
        debug!("Main actor thread and reth thread stopped");
        self.stop_guard.mark_stopped();
    }

    pub fn start_mining(&self) -> eyre::Result<()> {
        // start processing new blocks
        self.actor_addresses.start_mining()?;
        Ok(())
    }

    pub fn get_port(&self) -> u16 {
        self.config.port
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

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
            panic!("IrysNodeCtx must be stopped before all instances are dropped");
        }
    }
}

impl Clone for StopGuard {
    fn clone(&self) -> Self {
        StopGuard(Arc::clone(&self.0))
    }
}

async fn start_reth_node<T: HasName + HasTableType>(
    task_executor: TaskExecutor,
    chainspec: ChainSpec,
    irys_config: Arc<IrysNodeConfig>,
    tables: &[T],
    sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
    irys_provider: IrysRethProvider,
    latest_block: u64,
    random_ports: bool,
) -> eyre::Result<NodeExitReason> {
    let node_handle = irys_reth_node_bridge::run_node(
        Arc::new(chainspec),
        task_executor,
        irys_config,
        tables,
        irys_provider,
        latest_block,
        random_ports,
    )
    .await
    .expect("expected reth node to have started");
    debug!("Reth node started");
    sender
        .send(node_handle.node.clone())
        .expect("unable to send reth node handle");

    node_handle.node_exit_future.await
}
/// Builder pattern for configuring and bootstrapping an Irys blockchain node.
#[derive(Clone)]
pub struct IrysNode {
    pub config: Config,
    pub irys_node_config: IrysNodeConfig,
    pub storage_config: StorageConfig,
    pub is_genesis: bool,
    pub data_exists: bool,
    pub vdf_config: VDFStepsConfig,
    pub epoch_config: EpochServiceConfig,
    pub storage_submodule_config: StorageSubmodulesConfig,
    pub difficulty_adjustment_config: DifficultyAdjustmentConfig,
    pub packing_config: PackingConfig,
    pub genesis_timestamp: u128,
}

impl IrysNode {
    /// Creates a new node builder instance.
    pub async fn new(config: Config, is_genesis: bool) -> Self {
        let storage_config = StorageConfig::new(&config);
        let irys_node_config = IrysNodeConfig::new(&config);
        let data_exists = Self::blockchain_data_exists(&irys_node_config.base_directory);
        let vdf_config = VDFStepsConfig::new(&config);
        let epoch_config = EpochServiceConfig::new(&config);
        let difficulty_adjustment_config = DifficultyAdjustmentConfig::new(&config);
        let packing_config = PackingConfig::new(&config);

        // this populates the base directory
        let storage_submodule_config =
            StorageSubmodulesConfig::load(irys_node_config.instance_directory().clone()).unwrap();

        let (_, irys_genesis) = irys_node_config.chainspec_builder.build();
        let irys_genesis_block: Arc<IrysBlockHeader> = if is_genesis {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            let irys_genesis = IrysBlockHeader {
                diff: calculate_initial_difficulty(
                    &difficulty_adjustment_config,
                    &storage_config,
                    // TODO: where does this magic constant come from?
                    3,
                )
                .expect("valid calculated initial difficulty"),
                timestamp: now.as_millis(),
                last_diff_timestamp: now.as_millis(),
                ..irys_genesis
            };
            info!(
                "genesis generated by this node at startup {:?}",
                &irys_genesis
            );
            Arc::new(irys_genesis)
        } else {
            info!("fetching genesis block from trusted peer");
            let awc_client = awc::Client::new();
            fetch_genesis_block(
                config
                    .trusted_peers
                    .first()
                    .expect("expected at least one trusted peer in config"),
                &awc_client,
            )
            .await
            .expect("expected genesis block from http api")
        };

        IrysNode {
            config,
            genesis_timestamp: irys_genesis_block.timestamp,
            data_exists,
            irys_node_config,
            storage_config,
            is_genesis,
            vdf_config,
            epoch_config,
            storage_submodule_config,
            difficulty_adjustment_config,
            packing_config,
        }
    }

    /// Checks if local blockchain data exists.
    fn blockchain_data_exists(base_dir: &PathBuf) -> bool {
        match fs::read_dir(base_dir) {
            // Are there any entries?
            Ok(mut entries) => entries.next().is_some(),
            // no entries in the directory
            Err(_) => false,
        }
    }

    /// Initializes the node (genesis or non-genesis).
    pub async fn start(&mut self) -> eyre::Result<IrysNodeCtx> {
        info!(miner_address = ?self.config.miner_address(), "Starting Irys Node");
        let (chain_spec, irys_genesis) = self.irys_node_config.chainspec_builder.build();

        // figure out the init mode
        let (latest_block_height_tx, latest_block_height_rx) = oneshot::channel::<u64>();
        match (self.data_exists, self.is_genesis) {
            (true, true) => eyre::bail!("You cannot start a genesis chain with existing data"),
            (false, _) => {
                // special handling for genesis node
                let commitments = get_genesis_commitments(&self.config);

                let mut irys_genesis = IrysBlockHeader {
                    diff: calculate_initial_difficulty(
                        &self.difficulty_adjustment_config,
                        &self.storage_config,
                        // TODO: where does this magic constant come from?
                        3,
                    )
                    .expect("valid calculated initial difficulty"),
                    timestamp: self.genesis_timestamp,
                    last_diff_timestamp: self.genesis_timestamp,
                    ..irys_genesis
                };
                add_genesis_commitments(&mut irys_genesis, &self.config);
                let irys_genesis_block = Arc::new(irys_genesis);

                // special handilng for genesis node
                self.init_genesis_thread(irys_genesis_block.clone(), commitments)?
                    .join()
                    .map_err(|_| eyre::eyre!("genesis init thread panicked"))?;
            }
            _ => {
                // no special handling for `peer` mode node
            }
        };

        // all async tasks will be run on a new tokio runtime
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let task_manager = TaskManager::new(tokio_runtime.handle().clone());

        // we create the listener here so we know the port before we start passing around `config`
        let listener = create_listener(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            self.config.port,
        ))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| eyre::eyre!("Error getting local address: {:?}", &e))?;
        // if `config.port` == 0, the assigned port will be random (decided by the OS)
        // we re-assign the configuration with the actual port here.
        let random_ports = if self.config.port == 0 {
            self.config.port = local_addr.port();
            true
        } else {
            false
        };

        // Common node startup logic (common for genesis and peer mode nodes)
        // There are a lot of cross dependencies between reth and irys components, the channels mediate the comms
        let (reth_shutdown_sender, reth_shutdown_receiver) = tokio::sync::mpsc::channel::<()>(1);
        let (main_actor_thread_shutdown_tx, main_actor_thread_shutdown_rx) =
            tokio::sync::mpsc::channel::<()>(1);
        let (vdf_sthutodwn_sender, vdf_sthutodwn_receiver) = mpsc::channel();
        let (reth_handle_sender, reth_handle_receiver) =
            oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
        let (irys_node_ctx_tx, irys_node_ctx_rx) = oneshot::channel::<IrysNodeCtx>();

        let irys_provider = irys_storage::reth_provider::create_provider();

        // init the services
        let actor_main_thread_handle = self.init_services_thread(
            latest_block_height_tx,
            reth_shutdown_sender,
            main_actor_thread_shutdown_rx,
            vdf_sthutodwn_sender,
            vdf_sthutodwn_receiver,
            reth_handle_receiver,
            irys_node_ctx_tx,
            &irys_provider,
            task_manager.executor(),
            listener,
        )?;

        // await the latest height to be reported
        let latest_height = latest_block_height_rx.await?;

        // start reth
        let reth_thread = self.init_reth_thread(
            reth_shutdown_receiver,
            main_actor_thread_shutdown_tx,
            reth_handle_sender,
            actor_main_thread_handle,
            irys_provider.clone(),
            chain_spec.clone(),
            latest_height,
            task_manager,
            tokio_runtime,
            random_ports,
        )?;

        let mut ctx = irys_node_ctx_rx.await?;
        ctx.reth_thread_handle = Some(reth_thread.into());
        // load peers from config into our database
        for peer_address in ctx.config.trusted_peers.clone() {
            let peer_list_entry = PeerListItem {
                address: PeerAddress {
                    api: peer_address,
                    gossip: peer_address, //FIXME this should be the gossip address and not a copy of the api address
                },
                ..Default::default()
            };

            if let Err(e) = ctx
                .actor_addresses
                .peer_list_service_addr
                .send(AddPeerMessage(peer_list_entry))
                .await
            {
                error!("Unable to send AddPeerMessage message {e}");
            };
        }

        // if we are an empty node joining an existing network
        if !self.data_exists && !self.is_genesis {
            sync_state_from_peers(
                ctx.config.trusted_peers.clone(),
                ctx.actor_addresses.block_discovery_addr.clone(),
                ctx.actor_addresses.mempool.clone(),
                ctx.actor_addresses.peer_list_service_addr.clone(),
            )
            .await?;
        }

        Ok(ctx)
    }

    fn init_genesis_thread(
        &self,
        irys_genesis: Arc<IrysBlockHeader>,
        commitments: Vec<CommitmentTransaction>,
    ) -> Result<JoinHandle<()>, eyre::Error> {
        let handle = std::thread::Builder::new()
            .name("genesis init system".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn({
                let node = (*self).clone();
                let irys_genesis = irys_genesis.clone();
                move || {
                    System::new().block_on(async move {
                        // bootstrap genesis
                        let node_config = Arc::new(node.irys_node_config.clone());
                        let block_index = BlockIndex::new()
                            .init(node_config.clone())
                            .await
                            .expect("initializing a new block index should be doable");
                        let block_index = Arc::new(RwLock::new(block_index));
                        let _block_index_service_actor = genesis_initialization(
                            &irys_genesis,
                            commitments,
                            node_config,
                            &block_index,
                            &node,
                        )
                        .await;
                        // optionally spawn other services to set up the base state
                    });
                }
            })?;
        Ok(handle)
    }

    fn init_services_thread(
        &self,
        latest_block_height_tx: oneshot::Sender<u64>,
        reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
        mut main_actor_thread_shutdown_rx: tokio::sync::mpsc::Receiver<()>,
        vdf_shutdown_sender: mpsc::Sender<()>,
        vdf_shutdown_receiver: mpsc::Receiver<()>,
        reth_handle_receiver: oneshot::Receiver<FullNode<RethNode, RethNodeAddOns>>,
        irys_node_ctx_tx: oneshot::Sender<IrysNodeCtx>,
        irys_provider: &Arc<RwLock<Option<IrysRethProviderInner>>>,
        task_exec: TaskExecutor,
        http_listener: TcpListener,
    ) -> Result<JoinHandle<RethNodeProvider>, eyre::Error> {
        let actor_main_thread_handle = std::thread::Builder::new()
            .name("actor-main-thread".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn({
                let node = (*self).clone();
                let irys_provider = irys_provider.clone();
                move || {
                    System::new().block_on(async move {
                        // read the latest block info
                        let node_config = Arc::new(node.irys_node_config.clone());
                        let (latest_block_height, block_index, latest_block) =
                            read_latest_block_data(node_config.clone()).await;
                        latest_block_height_tx
                            .send(latest_block_height)
                            .expect("to be able to send the latest block height");
                        let block_index_service_actor = node.init_block_index_service(&block_index);

                        // start the rest of the services
                        let (irys_node, actix_server, vdf_thread, arbiters, reth_node, gossip_service_handle) = node
                            .init_services(
                                reth_shutdown_sender,
                                vdf_shutdown_receiver,
                                reth_handle_receiver,
                                block_index,
                                latest_block.clone(),
                                irys_provider.clone(),
                                block_index_service_actor,
                                &task_exec,
                                http_listener
                            )
                            .await
                            .expect("initializng services should not fail");
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
                        for arbiter in arbiters {
                            arbiter.stop_and_join();
                        }
                        debug!("Actors stopped");

                        // Send shutdown signal
                        vdf_shutdown_sender.send(()).unwrap();

                        debug!("Waiting for VDF thread to finish");
                        // Wait for vdf thread to finish & save steps
                        vdf_thread.join().unwrap();

                        debug!("VDF thread finished");
                        reth_node
                    })
                }
            })?;
        Ok(actor_main_thread_handle)
    }

    fn init_reth_thread(
        &self,
        reth_shutdown_receiver: tokio::sync::mpsc::Receiver<()>,
        main_actor_thread_shutdown_tx: tokio::sync::mpsc::Sender<()>,
        reth_handle_sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
        actor_main_thread_handle: JoinHandle<RethNodeProvider>,
        irys_provider: IrysRethProvider,
        reth_chainspec: ChainSpec,
        latest_block_height: u64,
        mut task_manager: TaskManager,
        tokio_runtime: Runtime,
        random_ports: bool,
    ) -> eyre::Result<JoinHandle<()>> {
        let node_config = Arc::new(self.irys_node_config.clone());
        let reth_thread_handler = std::thread::Builder::new()
            .name("reth-thread".to_string())
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                let exec = task_manager.executor();
                let run_reth_until_ctrl_c_or_signal = async || {
                    _ = run_to_completion_or_panic(
                        &mut task_manager,
                        // todo we can simplify things if we use `irys_reth_node_bridge::run_node` directly
                        //      Then we can drop the channel
                        run_until_ctrl_c_or_channel_message(
                            start_reth_node(
                                exec,
                                reth_chainspec,
                                node_config,
                                IrysTables::ALL,
                                reth_handle_sender,
                                irys_provider.clone(),
                                latest_block_height,
                                random_ports,
                            ),
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
        &self,
        reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
        vdf_shutdown_receiver: std::sync::mpsc::Receiver<()>,
        reth_handle_receiver: oneshot::Receiver<FullNode<RethNode, RethNodeAddOns>>,
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        latest_block: Arc<IrysBlockHeader>,
        irys_provider: IrysRethProvider,
        block_index_service_actor: Addr<BlockIndexService>,
        task_exec: &TaskExecutor,
        http_listener: TcpListener,
    ) -> eyre::Result<(
        IrysNodeCtx,
        Server,
        JoinHandle<()>,
        Vec<ArbiterHandle>,
        RethNodeProvider,
        ServiceHandleWithShutdownSignal,
    )> {
        let node_config = Arc::new(self.irys_node_config.clone());

        // init Irys DB
        let irys_db = init_irys_db(&node_config)?;

        // initialize the databases
        let (reth_node, reth_db) = init_reth_db(reth_handle_receiver, &irys_db).await?;
        debug!("Reth DB initiailsed");

        // start services
        let (service_senders, receivers) = ServiceSenders::new();
        let _handle = ChunkCacheService::spawn_service(
            &task_exec,
            irys_db.clone(),
            receivers.chunk_cache,
            self.config.clone(),
        );
        debug!("Chunk cache initiailsed");

        // start reth service
        let (reth_service_actor, reth_arbiter) = init_reth_service(&irys_db, &reth_node);
        debug!("Reth Service Actor initiailsed");

        // update reth service about the latest block data it must use
        reth_service_actor
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Evm(latest_block.evm_block_hash),
                confirmed_hash: Some(BlockHashType::Evm(latest_block.evm_block_hash)),
                finalized_hash: None,
            })
            .await??;
        debug!("Reth Service Actor updated about fork choice");

        let block_index_guard = block_index_service_actor
            .send(GetBlockIndexGuardMessage)
            .await?;

        // start the broadcast mimning service
        let (broadcast_mining_actor, broadcast_arbiter) = init_broadcaster_service();

        // start the epoch service
        let (storage_module_infos, epoch_service_actor) = self
            .init_epoch_service(&irys_db, &block_index_guard)
            .await?;

        // Retrieve Partition assignment
        let partition_assignments_guard = epoch_service_actor
            .send(GetPartitionAssignmentsGuardMessage)
            .await?;
        let storage_modules = self.init_storage_modules(storage_module_infos);

        let (gossip_service, gossip_tx) = irys_gossip_service::GossipService::new(
            &self.config.gossip_service_bind_ip,
            self.config.gossip_service_port,
            irys_db.clone(),
        );

        // start the block tree service
        let (block_tree_service, block_tree_arbiter) = self.init_block_tree_service(
            &block_index,
            &irys_db,
            &service_senders,
            &block_index_guard,
        );
        let block_tree_guard = block_tree_service.send(GetBlockTreeGuardMessage).await?;

        // Spawn EMA service
        let _handle = EmaService::spawn_service(
            &task_exec,
            block_tree_guard.clone(),
            receivers.ema,
            &self.config,
        );

        // Spawn peer list service
        let (peer_list_addr, peer_list_arbiter) = init_peer_list_service(&irys_db);

        // Spawn the mempool service
        let (mempool_service, mempool_arbiter) = self.init_mempools_service(
            &node_config,
            &irys_db,
            &reth_node,
            reth_db,
            &storage_modules,
            &block_tree_guard,
            gossip_tx.clone(),
        );

        // spawn the chunk migration service
        self.init_chunk_migration_service(
            block_index,
            &irys_db,
            &service_senders,
            &storage_modules,
        );

        // spawn the vdf service
        let vdf_service = self.init_vdf_service(&irys_db, &block_index_guard);
        let vdf_steps_guard = vdf_service.send(GetVdfStateMessage).await?;

        // spawn the validation service
        let validation_arbiter = self.init_validation_service(
            &block_index_guard,
            &partition_assignments_guard,
            &vdf_steps_guard,
        );

        // spawn block discovery
        let (block_discovery, block_discovery_arbiter) = self.init_block_discovery_service(
            &irys_db,
            &service_senders,
            &block_index_guard,
            partition_assignments_guard,
            &vdf_steps_guard,
            gossip_tx.clone(),
        );

        let gossip_service_handle = gossip_service.run(
            mempool_service.clone(),
            block_discovery.clone(),
            irys_api_client::IrysApiClient::new(),
            task_exec,
        )?;

        // set up the price oracle
        let price_oracle = self.init_price_oracle();

        // set up the block producer
        let (block_producer_addr, block_producer_arbiter) = self.init_block_producer(
            &irys_db,
            &reth_node,
            &service_senders,
            &epoch_service_actor,
            &block_tree_guard,
            &mempool_service,
            &vdf_steps_guard,
            block_discovery.clone(),
            price_oracle,
        );

        let (global_step_number, seed) = vdf_steps_guard.read().get_last_step_and_seed();
        let seed = seed
            .map(|x| x.0)
            .unwrap_or(latest_block.vdf_limiter_info.seed);

        // set up packing actor
        let (atomic_global_step_number, packing_actor_addr) =
            self.init_packing_actor(global_step_number, &reth_node, &storage_modules);

        // set up storage modules
        let (part_actors, part_arbiters) = self.init_partition_mining_actor(
            &irys_db,
            &storage_modules,
            &vdf_steps_guard,
            &block_producer_addr,
            &atomic_global_step_number,
            &packing_actor_addr,
        );
        broadcast_mining_actor
            .send(BroadcastDifficultyUpdate(latest_block.clone()))
            .await?;

        // set up the vdf thread
        let vdf_thread_handler = self.init_vdf_thread(
            vdf_shutdown_receiver,
            latest_block,
            seed,
            global_step_number,
            broadcast_mining_actor,
            vdf_service,
            atomic_global_step_number,
        );

        // set up chunk provider
        let chunk_provider = self.init_chunk_provider(storage_modules);
        // set up IrysNodeCtx
        let irys_node_ctx = IrysNodeCtx {
            actor_addresses: ActorAddresses {
                partitions: part_actors,
                block_discovery_addr: block_discovery,
                block_producer: block_producer_addr,
                packing: packing_actor_addr,
                mempool: mempool_service.clone(),
                block_index: block_index_service_actor,
                epoch_service: epoch_service_actor,
                peer_list_service_addr: peer_list_addr,
            },
            reth_handle: reth_node.clone(),
            db: irys_db.clone(),
            node_config: node_config.clone(),
            chunk_provider: chunk_provider.clone(),
            block_index_guard: block_index_guard.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            vdf_config: self.vdf_config.clone(),
            storage_config: self.storage_config.clone(),
            service_senders: service_senders.clone(),
            reth_shutdown_sender,
            reth_thread_handle: None,
            block_tree_guard: block_tree_guard.clone(),
            config: Arc::new(self.config.clone()),
            stop_guard: StopGuard::new(),
        };

        let mut service_arbiters = Vec::new();
        service_arbiters.push(ArbiterHandle::new(
            block_producer_arbiter,
            "block_producer_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            broadcast_arbiter,
            "broadcast_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            block_discovery_arbiter,
            "block_discovery_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            validation_arbiter,
            "validation_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            block_tree_arbiter,
            "block_tree_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            peer_list_arbiter,
            "peer_list_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(
            mempool_arbiter,
            "mempool_arbiter".to_string(),
        ));
        service_arbiters.push(ArbiterHandle::new(reth_arbiter, "reth_arbiter".to_string()));
        service_arbiters.extend(
            part_arbiters
                .into_iter()
                .map(|x| ArbiterHandle::new(x, "partition_arbiter".to_string())),
        );

        let server = run_server(
            ApiState {
                ema_service: service_senders.ema.clone(),
                mempool: mempool_service,
                chunk_provider: chunk_provider.clone(),
                db: irys_db,
                reth_provider: reth_node.clone(),
                block_tree: block_tree_guard.clone(),
                block_index: block_index_guard.clone(),
                config: self.config.clone(),
                reth_http_url: reth_node
                    .rpc_server_handle()
                    .http_url()
                    .expect("Missing reth rpc url!"),
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
            service_arbiters,
            reth_node,
            gossip_service_handle,
        ))
    }

    fn init_chunk_provider(&self, storage_modules: Vec<Arc<StorageModule>>) -> Arc<ChunkProvider> {
        let chunk_provider =
            ChunkProvider::new(self.storage_config.clone(), storage_modules.clone());
        let chunk_provider = Arc::new(chunk_provider);
        chunk_provider
    }

    fn init_vdf_thread(
        &self,
        vdf_sthutodwn_receiver: mpsc::Receiver<()>,
        latest_block: Arc<IrysBlockHeader>,
        seed: H256,
        global_step_number: u64,
        broadcast_mining_actor: actix::Addr<BroadcastMiningService>,
        vdf_service: actix::Addr<VdfService>,
        atomic_global_step_number: Arc<AtomicU64>,
    ) -> JoinHandle<()> {
        let vdf_reset_seed = latest_block.vdf_limiter_info.seed;
        // FIXME: this should be controlled via a config parameter rather than relying on test-only artifact generation
        // we can't use `cfg!(test)` to detect integration tests, so we check that the path is of form `(...)/.tmp/<random folder>`
        let is_test = self
            .irys_node_config
            .base_directory
            .parent()
            .is_some_and(|p| p.ends_with(".tmp"));
        let vdf_thread_handler = std::thread::spawn({
            let vdf_config = self.vdf_config.clone();
            move || {
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

                // TODO: these channels are unused
                let (_new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();
                run_vdf(
                    vdf_config,
                    global_step_number,
                    seed,
                    vdf_reset_seed,
                    new_seed_rx,
                    vdf_sthutodwn_receiver,
                    broadcast_mining_actor.clone(),
                    vdf_service.clone(),
                    atomic_global_step_number.clone(),
                )
            }
        });
        vdf_thread_handler
    }

    fn init_partition_mining_actor(
        &self,
        irys_db: &DatabaseProvider,
        storage_modules: &Vec<Arc<StorageModule>>,
        vdf_steps_guard: &VdfStepsReadGuard,
        block_producer_addr: &actix::Addr<BlockProducerActor>,
        atomic_global_step_number: &Arc<AtomicU64>,
        packing_actor_addr: &actix::Addr<PackingActor>,
    ) -> (Vec<actix::Addr<PartitionMiningActor>>, Vec<Arbiter>) {
        let mut part_actors = Vec::new();
        let mut arbiters = Vec::new();
        for sm in storage_modules {
            let partition_mining_actor = PartitionMiningActor::new(
                self.config.miner_address(),
                irys_db.clone(),
                block_producer_addr.clone().recipient(),
                packing_actor_addr.clone().recipient(),
                sm.clone(),
                false, // do not start mining automatically
                vdf_steps_guard.clone(),
                atomic_global_step_number.clone(),
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
        for sm in storage_modules {
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
        &self,
        global_step_number: u64,
        reth_node: &RethNodeProvider,
        storage_modules: &Vec<Arc<StorageModule>>,
    ) -> (Arc<AtomicU64>, actix::Addr<PackingActor>) {
        let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));
        let sm_ids = storage_modules.iter().map(|s| (*s).id).collect();
        let packing_actor_addr = PackingActor::new(
            reth_node.task_executor.clone(),
            sm_ids,
            self.packing_config.clone(),
        )
        .start();
        (atomic_global_step_number, packing_actor_addr)
    }

    fn init_block_producer(
        &self,
        irys_db: &DatabaseProvider,
        reth_node: &RethNodeProvider,
        service_senders: &ServiceSenders,
        epoch_service_actor: &actix::Addr<EpochServiceActor>,
        block_tree_guard: &BlockTreeReadGuard,
        mempool_service: &actix::Addr<MempoolService>,
        vdf_steps_guard: &VdfStepsReadGuard,
        block_discovery: actix::Addr<BlockDiscoveryActor>,
        price_oracle: Arc<IrysPriceOracle>,
    ) -> (actix::Addr<BlockProducerActor>, Arbiter) {
        let block_producer_arbiter = Arbiter::new();
        let block_producer_actor = BlockProducerActor {
            db: irys_db.clone(),
            mempool_addr: mempool_service.clone(),
            block_discovery_addr: block_discovery,
            epoch_service: epoch_service_actor.clone(),
            reth_provider: reth_node.clone(),
            storage_config: self.storage_config.clone(),
            difficulty_config: self.difficulty_adjustment_config.clone(),
            vdf_config: self.vdf_config.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            block_tree_guard: block_tree_guard.clone(),
            epoch_config: self.epoch_config.clone(),
            price_oracle,
            service_senders: service_senders.clone(),
        };
        let block_producer_addr =
            BlockProducerActor::start_in_arbiter(&block_producer_arbiter.handle(), |_| {
                block_producer_actor
            });
        (block_producer_addr, block_producer_arbiter)
    }

    fn init_price_oracle(&self) -> Arc<IrysPriceOracle> {
        let price_oracle = match self.config.oracle_config {
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
        &self,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_index_guard: &BlockIndexReadGuard,
        partition_assignments_guard: irys_actors::epoch_service::PartitionAssignmentsReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
        gossip_sender: tokio::sync::mpsc::Sender<GossipData>,
    ) -> (actix::Addr<BlockDiscoveryActor>, Arbiter) {
        let block_discovery_actor = BlockDiscoveryActor {
            block_index_guard: block_index_guard.clone(),
            partition_assignments_guard: partition_assignments_guard.clone(),
            storage_config: self.storage_config.clone(),
            difficulty_config: self.difficulty_adjustment_config.clone(),
            db: irys_db.clone(),
            vdf_config: self.vdf_config.clone(),
            vdf_steps_guard: vdf_steps_guard.clone(),
            service_senders: service_senders.clone(),
            gossip_sender,
        };
        let block_discovery_arbiter = Arbiter::new();
        let block_discovery =
            BlockDiscoveryActor::start_in_arbiter(&block_discovery_arbiter.handle(), |_| {
                block_discovery_actor
            });
        (block_discovery, block_discovery_arbiter)
    }

    fn init_validation_service(
        &self,
        block_index_guard: &BlockIndexReadGuard,
        partition_assignments_guard: &irys_actors::epoch_service::PartitionAssignmentsReadGuard,
        vdf_steps_guard: &VdfStepsReadGuard,
    ) -> Arbiter {
        let validation_service = ValidationService::new(
            block_index_guard.clone(),
            partition_assignments_guard.clone(),
            vdf_steps_guard.clone(),
            self.storage_config.clone(),
            self.vdf_config.clone(),
        );
        let validation_arbiter = Arbiter::new();
        let validation_service =
            ValidationService::start_in_arbiter(&validation_arbiter.handle(), |_| {
                validation_service
            });
        SystemRegistry::set(validation_service);
        validation_arbiter
    }

    fn init_vdf_service(
        &self,
        irys_db: &DatabaseProvider,
        block_index_guard: &BlockIndexReadGuard,
    ) -> actix::Addr<VdfService> {
        let vdf_service_actor =
            VdfService::new(block_index_guard.clone(), irys_db.clone(), &self.config);
        let vdf_service = vdf_service_actor.start();
        SystemRegistry::set(vdf_service.clone());
        vdf_service
    }

    fn init_chunk_migration_service(
        &self,
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        storage_modules: &Vec<Arc<StorageModule>>,
    ) {
        let chunk_migration_service = ChunkMigrationService::new(
            block_index.clone(),
            self.storage_config.clone(),
            storage_modules.clone(),
            irys_db.clone(),
            service_senders.clone(),
        );
        SystemRegistry::set(chunk_migration_service.start());
    }

    fn init_mempools_service(
        &self,
        node_config: &Arc<IrysNodeConfig>,
        irys_db: &DatabaseProvider,
        reth_node: &RethNodeProvider,
        reth_db: irys_database::db::RethDbWrapper,
        storage_modules: &Vec<Arc<StorageModule>>,
        block_tree_guard: &BlockTreeReadGuard,
        gossip_tx: tokio::sync::mpsc::Sender<GossipData>,
    ) -> (actix::Addr<MempoolService>, Arbiter) {
        let mempool_service = MempoolService::new(
            irys_db.clone(),
            reth_db.clone(),
            reth_node.task_executor.clone(),
            node_config.mining_signer.clone(),
            self.storage_config.clone(),
            storage_modules.clone(),
            block_tree_guard.clone(),
            &self.config,
            gossip_tx,
        );
        let mempool_arbiter = Arbiter::new();
        let mempool_service =
            MempoolService::start_in_arbiter(&mempool_arbiter.handle(), |_| mempool_service);
        SystemRegistry::set(mempool_service.clone());
        (mempool_service, mempool_arbiter)
    }

    fn init_block_tree_service(
        &self,
        block_index: &Arc<RwLock<BlockIndex<Initialized>>>,
        irys_db: &DatabaseProvider,
        service_senders: &ServiceSenders,
        block_index_guard: &BlockIndexReadGuard,
    ) -> (actix::Addr<BlockTreeService>, Arbiter) {
        let block_tree_service = BlockTreeService::new(
            irys_db.clone(),
            block_index.clone(),
            &self.config.miner_address(),
            block_index_guard.clone(),
            self.storage_config.clone(),
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
        &self,
        storage_module_infos: Vec<irys_storage::StorageModuleInfo>,
    ) -> Vec<Arc<StorageModule>> {
        let mut storage_modules = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(
                StorageModule::new(
                    &self.irys_node_config.storage_module_dir(),
                    &info,
                    self.storage_config.clone(),
                )
                // TODO: remove this unwrap
                .unwrap(),
            );
            storage_modules.push(arc_module.clone());
        }
        storage_modules
    }

    async fn init_epoch_service(
        &self,
        irys_db: &DatabaseProvider,
        block_index_guard: &BlockIndexReadGuard,
    ) -> Result<
        (
            Vec<irys_storage::StorageModuleInfo>,
            actix::Addr<EpochServiceActor>,
        ),
        eyre::Error,
    > {
        let mut epoch_service = EpochServiceActor::new(
            self.epoch_config.clone(),
            &self.config,
            block_index_guard.clone(),
        );
        let storage_module_infos = epoch_service
            .initialize(irys_db, self.storage_submodule_config.clone())
            .await?;
        let epoch_service_actor = epoch_service.start();
        Ok((storage_module_infos, epoch_service_actor))
    }

    fn init_block_index_service(
        &self,
        block_index: &Arc<RwLock<BlockIndex<Initialized>>>,
    ) -> actix::Addr<BlockIndexService> {
        let block_index_service =
            BlockIndexService::new(block_index.clone(), self.storage_config.clone());
        let block_index_service_actor = block_index_service.start();
        SystemRegistry::set(block_index_service_actor.clone());
        block_index_service_actor
    }
}

async fn read_latest_block_data(
    node_config: Arc<IrysNodeConfig>,
) -> (
    u64,
    Arc<RwLock<BlockIndex<Initialized>>>,
    Arc<IrysBlockHeader>,
) {
    let block_index = BlockIndex::new()
        .init(node_config.clone())
        .await
        .expect("to init block index");
    let latest_block_index = block_index
        .get_latest_item()
        .cloned()
        .expect("the block index must have at least one entry");
    let latest_block_height = block_index.latest_height();
    let block_index = Arc::new(RwLock::new(block_index));
    let irys_db = init_irys_db(&node_config).expect("could not open irys db");
    let latest_block = Arc::new(
        database::block_header_by_hash(
            &irys_db.tx().unwrap(),
            &latest_block_index.block_hash,
            false,
        )
        .unwrap()
        .unwrap(),
    );
    drop(irys_db);
    (latest_block_height, block_index, latest_block)
}

async fn genesis_initialization(
    irys_genesis: &Arc<IrysBlockHeader>,
    commitments: Vec<CommitmentTransaction>,
    node_config: Arc<IrysNodeConfig>,
    block_index: &Arc<RwLock<BlockIndex<Initialized>>>,
    node: &IrysNode,
) -> Addr<BlockIndexService> {
    // write the genesis block to the irys db
    let irys_db = init_irys_db(&node_config).expect("could not open irys db");
    irys_db
        .update_eyre(|tx| irys_database::insert_block_header(tx, irys_genesis))
        .expect("genesis db data could not be written");

    // Add the commitments to the db
    let tx = irys_db
        .tx_mut()
        .expect("to create a mutable mdbx transaction");
    for commitment in &commitments {
        insert_commitment_tx(&tx, commitment).expect("inserting commitment tx should succeed");
    }
    // Make sure the database transaction completes before dropping the db reference
    tx.inner
        .commit()
        .expect("to commit the mdbx transaction to the db");

    drop(irys_db);

    // start block index service, we need to preconfigure the initial finalized block
    let block_index_service_actor = node.init_block_index_service(block_index);
    let msg = BlockFinalizedMessage {
        block_header: irys_genesis.clone(),
        all_txs: Arc::new(vec![]),
    };
    block_index_service_actor
        .send(msg)
        .await
        .expect("to send the genesis finalization msg")
        .expect("block index to accept the genesis finalization block");
    block_index_service_actor
}

fn init_peer_list_service(irys_db: &DatabaseProvider) -> (actix::Addr<PeerListService>, Arbiter) {
    let peer_list_arbiter = Arbiter::new();
    let peer_list_service = PeerListService::new(irys_db.clone());
    let peer_list_service =
        PeerListService::start_in_arbiter(&peer_list_arbiter.handle(), |_| peer_list_service);
    SystemRegistry::set(peer_list_service.clone());
    (peer_list_service, peer_list_arbiter)
}

fn init_broadcaster_service() -> (actix::Addr<BroadcastMiningService>, Arbiter) {
    let broadcast_arbiter = Arbiter::new();
    let broadcast_mining_actor =
        BroadcastMiningService::start_in_arbiter(&broadcast_arbiter.handle(), |_| {
            BroadcastMiningService::default()
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
    reth_handle_receiver: oneshot::Receiver<FullNode<RethNode, RethNodeAddOns>>,
    irys_db: &DatabaseProvider,
) -> Result<(RethNodeProvider, irys_database::db::RethDbWrapper), eyre::Error> {
    let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await?));
    let reth_db = reth_node.provider.database.db.clone();
    check_db_version_and_run_migrations_if_needed(&reth_db, irys_db)?;
    Ok((reth_node, reth_db))
}

fn init_irys_db(node_config: &IrysNodeConfig) -> Result<DatabaseProvider, eyre::Error> {
    let irys_db_env =
        open_or_create_irys_consensus_data_db(&node_config.irys_consensus_data_dir())?;
    let irys_db = DatabaseProvider(Arc::new(irys_db_env));
    debug!("Irys DB initiailsed");
    Ok(irys_db)
}
