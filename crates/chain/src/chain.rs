use crate::arbiter_handle::{ArbiterHandle, CloneableJoinHandle};
use crate::vdf::run_vdf;
use ::irys_database::{tables::IrysTables, BlockIndex, Initialized};
use actix::{Actor, System, SystemRegistry};
use actix::{Arbiter, SystemService};
use alloy_eips::BlockNumberOrTag;
use irys_actors::cache_service::ChunkCacheService;
use irys_actors::ema_service::EmaService;
use irys_actors::packing::PackingConfig;
use irys_actors::peer_list_service::PeerListService;
use irys_actors::reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor};
use irys_actors::services::ServiceSenders;
use irys_actors::{
    block_discovery::BlockDiscoveryActor,
    block_index_service::{BlockIndexReadGuard, BlockIndexService, GetBlockIndexGuardMessage},
    block_producer::BlockProducerActor,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    broadcast_mining_service::{BroadcastDifficultyUpdate, BroadcastMiningService},
    chunk_migration_service::ChunkMigrationService,
    epoch_service::{
        EpochServiceActor, EpochServiceConfig, GetLedgersGuardMessage,
        GetPartitionAssignmentsGuardMessage,
    },
    mempool_service::MempoolService,
    mining::PartitionMiningActor,
    packing::{PackingActor, PackingRequest},
    validation_service::ValidationService,
    vdf_service::{GetVdfStateMessage, VdfService},
    ActorAddresses, BlockFinalizedMessage,
};
use irys_api_server::{run_server, ApiState};
use irys_config::{IrysNodeConfig, StorageSubmodulesConfig};
use irys_database::database;
use irys_database::migration::check_db_version_and_run_migrations_if_needed;
use irys_packing::{PackingType, PACKING_TYPE};
use irys_price_oracle::mock_oracle::MockOracle;
use irys_price_oracle::IrysPriceOracle;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_storage::{
    reth_provider::{IrysRethProvider, IrysRethProviderInner},
    ChunkProvider, ChunkType, StorageModule, StorageModuleVec,
};
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, vdf_config::VDFStepsConfig,
    StorageConfig, CHUNK_SIZE, H256,
};
use irys_types::{Config, DifficultyAdjustmentConfig, OracleConfig, PartitionChunkRange};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth::rpc::eth::EthApiServer as _;
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c_or_channel_message};
use reth_db::{Database as _, HasName, HasTableType};
use std::sync::atomic::AtomicU64;
use std::{
    fs,
    sync::{mpsc, Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::Handle,
    sync::oneshot::{self},
};
use tracing::{debug, error, info};

pub async fn start(config: Config) -> eyre::Result<IrysNodeCtx> {
    let irys_node_config = IrysNodeConfig::new(&config);
    let storage_config = StorageConfig::new(&config);

    start_irys_node(irys_node_config, storage_config, config).await
}

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub config: Arc<IrysNodeConfig>,
    pub chunk_provider: Arc<ChunkProvider>,
    pub block_index_guard: BlockIndexReadGuard,
    pub vdf_steps_guard: VdfStepsReadGuard,
    pub vdf_config: VDFStepsConfig,
    pub storage_config: StorageConfig,
    pub service_senders: ServiceSenders,
    // Shutdown channels
    pub reth_shutdown_sender: tokio::sync::mpsc::Sender<()>,
    // Thread handles spawned by the start function
    pub reth_thread_handle: Option<CloneableJoinHandle<()>>,
}

impl IrysNodeCtx {
    pub async fn stop(self) {
        debug!("Sending shutdown signal to reth thread");
        // Shutting down reth node will propagate to the main actor thread eventually
        let _ = self.reth_shutdown_sender.send(()).await;
        let _ = self.reth_thread_handle.unwrap().join();
        debug!("Main actor thread and reth thread stopped");
    }
}

pub async fn start_irys_node(
    node_config: IrysNodeConfig,
    storage_config: StorageConfig,
    config: Config,
) -> eyre::Result<IrysNodeCtx> {
    info!("Using directory {:?}", &node_config.base_directory);

    // Delete the .irys folder if we are not persisting data on restart
    let base_dir = node_config.instance_directory();
    if fs::exists(&base_dir).unwrap_or(false) && config.reset_state_on_restart {
        // remove existing data directory as storage modules are packed with a different miner_signer generated next
        info!("Removing .irys folder {:?}", &base_dir);
        fs::remove_dir_all(&base_dir).expect("Unable to remove .irys folder");
    }

    // Autogenerates the ".irys_submodules.toml" in dev mode
    let storage_module_config = StorageSubmodulesConfig::load(base_dir.clone()).unwrap();

    if PACKING_TYPE != PackingType::CPU && storage_config.chunk_size != CHUNK_SIZE {
        error!("GPU packing only supports chunk size {}!", CHUNK_SIZE)
    }

    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
    let (irys_node_handle_sender, irys_node_handle_receiver) = oneshot::channel::<IrysNodeCtx>();
    let (reth_chainspec, mut irys_genesis) = node_config.chainspec_builder.build();
    let arc_config = Arc::new(node_config);
    let difficulty_adjustment_config = DifficultyAdjustmentConfig::new(&config);

    // TODO: Hard coding 3 for storage module count isn't great here,
    // eventually we'll want to relate this to the genesis config
    irys_genesis.diff =
        calculate_initial_difficulty(&difficulty_adjustment_config, &storage_config, 3).unwrap();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    irys_genesis.timestamp = now.as_millis();
    irys_genesis.last_diff_timestamp = irys_genesis.timestamp;
    let arc_genesis = Arc::new(irys_genesis);

    let mut storage_modules: StorageModuleVec = Vec::new();

    let at_genesis;
    let latest_block_index: Option<irys_database::BlockIndexItem>;

    let latest_block_height;
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new({
        let idx = BlockIndex::default();
        let i = idx.init(arc_config.clone()).await.unwrap();

        at_genesis = i.get_item(0).is_none();
        if at_genesis {
            debug!("At genesis!")
        } else {
            debug!("Not at genesis!")
        }
        latest_block_index = i.get_latest_item().cloned();
        latest_block_height = i.latest_height();
        debug!(
            "Requesting prune until block height {}",
            &latest_block_height
        );

        i
    }));

    let cloned_arc = arc_config.clone();

    // Spawn thread and runtime for actors
    let arc_config_copy = arc_config.clone();
    let irys_provider = irys_storage::reth_provider::create_provider();

    // clone as this gets `move`d into the thread
    let irys_provider_1 = irys_provider.clone();

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut task_manager = TaskManager::new(tokio_runtime.handle().clone());
    let task_exec = task_manager.executor();
    let (reth_shutdown_sender, reth_shutdown_receiver) = tokio::sync::mpsc::channel::<()>(1);
    let (main_actor_thread_shutdown_tx, mut main_actor_thread_shutdown_rx) =
        tokio::sync::mpsc::channel::<()>(1);

    let actor_main_thread_handle = std::thread::Builder::new()
        .name("actor-main-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let node_config = arc_config_copy.clone();
            let reth_node_handle = System::new().block_on(async move {
                let mut arbiters = Vec::new();
                let irys_db_env = open_or_create_irys_consensus_data_db(
                    &arc_config.irys_consensus_data_dir(),
                ).unwrap();
                // the RethNodeHandle doesn't *need* to be Arc, but it will reduce the copy cost
                let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await.unwrap()));
                let reth_db = reth_node.provider.database.db.clone();
                let irys_db = DatabaseProvider(Arc::new(irys_db_env));
                let vdf_config = VDFStepsConfig::new(&config);

                check_db_version_and_run_migrations_if_needed(&reth_db, &irys_db).unwrap();

                let (service_senders, receivers) = ServiceSenders::new();
                let _handle = ChunkCacheService::spawn_service(&task_exec, irys_db.clone(), receivers.chunk_cache, config.clone());

                let latest_block = latest_block_index
                    .map(|b| {
                        database::block_header_by_hash(&irys_db.tx().unwrap(), &b.block_hash)
                            .unwrap()
                            .unwrap()
                    })
                    .map(Arc::new)
                    .unwrap_or(arc_genesis.clone());

                // Initialize the epoch_service actor to handle partition ledger assignments
                let epoch_config = EpochServiceConfig::new(&config);

                let miner_address = node_config.mining_signer.address();
                debug!("Miner address {:?}", miner_address);

                let reth_service = RethServiceActor::new(reth_node.clone(), irys_db.clone());
                let reth_arbiter = Arbiter::new();
                SystemRegistry::set(RethServiceActor::start_in_arbiter(
                    &reth_arbiter.handle(),
                    |_| reth_service,
                ));
                arbiters.push(reth_arbiter.into());

                debug!(
                    "JESSEDEBUG setting head to block {} ({})",
                    &latest_block.evm_block_hash, &latest_block.height
                );

                {
                    let context = RethNodeContext::new(reth_node.clone().into())
                        .await
                        .map_err(|e| eyre::eyre!("Error connecting to Reth: {}", e))
                        .unwrap();

                    let latest = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Latest, false)
                        .await;

                    let safe = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Safe, false)
                        .await;

                    let finalized = context
                        .rpc
                        .inner
                        .eth_api()
                        .block_by_number(BlockNumberOrTag::Finalized, false)
                        .await;

                    debug!(
                        "JESSEDEBUG FCU S latest {:?}, safe {:?}, finalized {:?}",
                        &latest, &safe, &finalized
                    );


                    if latest.unwrap().unwrap().header.number != latest_block.height {
                        error!("Reth is out of sync with Irys block index! recovery will be attempted.")
                    };

                }

                RethServiceActor::from_registry()
                    .send(ForkChoiceUpdateMessage {
                        head_hash: BlockHashType::Evm(latest_block.evm_block_hash),
                        confirmed_hash: Some(BlockHashType::Evm(latest_block.evm_block_hash)),
                        finalized_hash: None,
                    })
                    .await
                    .unwrap()
                    .unwrap();

                // Initialize the block_index actor and tell it about the genesis block
                let block_index_actor =
                    BlockIndexService::new(block_index.clone(), storage_config.clone());
                SystemRegistry::set(block_index_actor.start());
                let block_index_actor_addr = BlockIndexService::from_registry();

                let block_index_guard = block_index_actor_addr
                    .send(GetBlockIndexGuardMessage)
                    .await
                    .unwrap();

                if at_genesis {
                    let msg = BlockFinalizedMessage {
                        block_header: arc_genesis.clone(),
                        all_txs: Arc::new(vec![]),
                    };
                    irys_db.update_eyre(|tx| irys_database::insert_block_header(tx, &arc_genesis))
                        .unwrap();
                    match block_index_actor_addr.send(msg).await {
                        Ok(_) => info!("Genesis block indexed"),
                        Err(_) => panic!("Failed to index genesis block"),
                    }
                }

                debug!("AT GENESIS {}", at_genesis);

                // need to start before the epoch service, as epoch service calls from_registry that triggers broadcast mining service initialization
                let broadcast_arbiter = Arbiter::new();
                let broadcast_mining_service =
                    BroadcastMiningService::start_in_arbiter(&broadcast_arbiter.handle(), |_| {
                        BroadcastMiningService::default()
                    });
                SystemRegistry::set(broadcast_mining_service.clone());

                let mut epoch_service = EpochServiceActor::new(epoch_config.clone(), &config, block_index_guard.clone());
                // initialize the epoch service from block 1
                let storage_module_infos = epoch_service.initialize(&irys_db, storage_module_config.clone()).await.expect("Failed to initialize epoch service");
                let epoch_service_actor_addr = epoch_service.start();

                // Retrieve ledger assignments
                let ledgers_guard = epoch_service_actor_addr
                    .send(GetLedgersGuardMessage)
                    .await
                    .unwrap();

                {
                    let ledgers = ledgers_guard.read();
                    debug!("ledgers: {:?}", ledgers);
                }

                let partition_assignments_guard = epoch_service_actor_addr
                    .send(GetPartitionAssignmentsGuardMessage)
                    .await
                    .unwrap();

                // Create a list of storage modules wrapping the storage files
                for info in storage_module_infos {
                    let arc_module = Arc::new(
                        StorageModule::new(
                            &arc_config.storage_module_dir(),
                            &info,
                            storage_config.clone(),
                        )
                        // TODO: remove this unwrap
                        .unwrap(),
                    );
                    storage_modules.push(arc_module.clone());
                }


                let block_tree_service = BlockTreeService::new(
                    irys_db.clone(),
                    block_index.clone(),
                    &miner_address,
                    block_index_guard.clone(),
                    storage_config.clone(),
                    service_senders.clone(),
                );
                let block_tree_arbiter = Arbiter::new();
                SystemRegistry::set(BlockTreeService::start_in_arbiter(
                    &block_tree_arbiter.handle(),
                    |_| block_tree_service,
                ));
                let block_tree_service = BlockTreeService::from_registry();
                arbiters.push(block_tree_arbiter.into());

                let block_tree_guard = block_tree_service
                    .send(GetBlockTreeGuardMessage)
                    .await
                    .unwrap();
                let _handle = EmaService::spawn_service(&task_exec, block_tree_guard.clone(),  receivers.ema, &config);

                let peer_list_service = PeerListService::new(irys_db.clone());
                let peer_list_arbiter = Arbiter::new();
                SystemRegistry::set(PeerListService::start_in_arbiter(
                    &peer_list_arbiter.handle(),
                    |_| peer_list_service,
                ));
                arbiters.push(peer_list_arbiter.into());

                let mempool_service = MempoolService::new(
                    irys_db.clone(),
                    reth_db.clone(),
                    reth_node.task_executor.clone(),
                    node_config.mining_signer.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                    block_tree_guard.clone(),
                    &config,
                );
                let mempool_arbiter = Arbiter::new();
                SystemRegistry::set(MempoolService::start_in_arbiter(
                    &mempool_arbiter.handle(),
                    |_| mempool_service,
                ));
                let mempool_addr = MempoolService::from_registry();
                arbiters.push(mempool_arbiter.into());

                let chunk_migration_service = ChunkMigrationService::new(
                    block_index.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                    irys_db.clone(),
                    service_senders.clone()
                );
                SystemRegistry::set(chunk_migration_service.start());

                let vdf_service_actor = VdfService::new(block_index_guard.clone(), irys_db.clone(), &config);
                let vdf_service = vdf_service_actor.start();
                SystemRegistry::set(vdf_service.clone());

                let vdf_steps_guard: VdfStepsReadGuard =
                    vdf_service.send(GetVdfStateMessage).await.unwrap();

                let validation_service = ValidationService::new(
                    block_index_guard.clone(),
                    partition_assignments_guard.clone(),
                    vdf_steps_guard.clone(),
                    storage_config.clone(),
                    vdf_config.clone(),
                );
                let validation_arbiter = Arbiter::new();
                SystemRegistry::set(ValidationService::start_in_arbiter(
                    &validation_arbiter.handle(),
                    |_| validation_service,
                ));
                arbiters.push(validation_arbiter.into());

                let (global_step_number, seed) = vdf_steps_guard.read().get_last_step_and_seed();
                info!("Starting at global step number: {}", global_step_number);

                let block_discovery_actor = BlockDiscoveryActor {
                    block_index_guard: block_index_guard.clone(),
                    partition_assignments_guard: partition_assignments_guard.clone(),
                    storage_config: storage_config.clone(),
                    difficulty_config: difficulty_adjustment_config,
                    db: irys_db.clone(),
                    vdf_config: vdf_config.clone(),
                    vdf_steps_guard: vdf_steps_guard.clone(),
                    service_senders: service_senders.clone(),
                };
                let block_discovery_arbiter = Arbiter::new();
                let block_discovery_addr = BlockDiscoveryActor::start_in_arbiter(
                    &block_discovery_arbiter.handle(),
                    |_| block_discovery_actor,
                );
                arbiters.push(block_discovery_arbiter.into());

                // set up the price oracle
                let price_oracle = match config.oracle_config {
                    OracleConfig::Mock { initial_price, percent_change, smoothing_interval } => {
                        IrysPriceOracle::MockOracle(MockOracle::new(initial_price, percent_change, smoothing_interval))
                    },
                    // note: depending on the oracle, it may require spawning an async background service.
                };
                let price_oracle = Arc::new(price_oracle);

                // set up the block producer
                let block_producer_arbiter = Arbiter::new();
                let block_producer_actor = BlockProducerActor {
                    db: irys_db.clone(),
                    mempool_addr: mempool_addr.clone(),
                    block_discovery_addr,
                    epoch_service: epoch_service_actor_addr.clone(),
                    reth_provider: reth_node.clone(),
                    storage_config: storage_config.clone(),
                    difficulty_config: difficulty_adjustment_config,
                    vdf_config: vdf_config.clone(),
                    vdf_steps_guard: vdf_steps_guard.clone(),
                    block_tree_guard: block_tree_guard.clone(),
                    epoch_config: epoch_config.clone(),
                    price_oracle,
                    service_senders: service_senders.clone(),
                };
                let block_producer_addr =
                    BlockProducerActor::start_in_arbiter(&block_producer_arbiter.handle(), |_| {
                        block_producer_actor
                    });
                arbiters.push(block_producer_arbiter.into());

                let mut part_actors = Vec::new();

                let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));

                let sm_ids = storage_modules.iter().map(|s| (*s).id).collect();

                let packing_actor_addr = PackingActor::new(
                    Handle::current(),
                    reth_node.task_executor.clone(),
                    sm_ids,
                    PackingConfig::new(&config),
                )
                .start();

                for sm in &storage_modules {
                    let partition_mining_actor = PartitionMiningActor::new(
                        miner_address,
                        irys_db.clone(),
                        block_producer_addr.clone().recipient(),
                        packing_actor_addr.clone().recipient(),
                        sm.clone(),
                        false, // do not start mining automatically
                        vdf_steps_guard.clone(),
                        atomic_global_step_number.clone(),
                    );
                    let part_arbiter = Arbiter::new();
                    part_actors.push(PartitionMiningActor::start_in_arbiter(
                        &part_arbiter.handle(),
                        |_| partition_mining_actor,
                    ));
                    arbiters.push(ArbiterHandle::new(part_arbiter));
                }

                // Yield to let actors process their mailboxes (and subscribe to the mining_broadcaster)
                tokio::task::yield_now().await;

                // request packing for uninitialized ranges
                for sm in &storage_modules {
                    let uninitialized = sm.get_intervals(ChunkType::Uninitialized);
                    debug!("ranges to pack: {:?}", &uninitialized);
                    let _ = uninitialized
                        .iter()
                        .map(|interval| {
                            packing_actor_addr.do_send(PackingRequest {
                                storage_module: sm.clone(),
                                chunk_range: PartitionChunkRange(*interval),
                            })
                        })
                        .collect::<Vec<()>>();
                }
                let part_actors_clone = part_actors.clone();

                // Let the partition actors know about the genesis difficulty
                broadcast_mining_service
                    .send(BroadcastDifficultyUpdate(latest_block.clone()))
                    .await
                    .unwrap();

                let (_new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();
                let (shutdown_tx, shutdown_rx) = mpsc::channel();

                let vdf_config2 = vdf_config.clone();
                let seed = seed.map_or(arc_genesis.vdf_limiter_info.output, |seed| seed.0);
                let vdf_reset_seed = latest_block.vdf_limiter_info.seed;

                info!(
                    ?seed,
                    ?global_step_number,
                    reset_seed = ?arc_genesis.vdf_limiter_info.seed,
                    "Starting VDF thread",
                );

                let vdf_thread_handler = std::thread::spawn(move || {
                    // Setup core affinity
                    let core_ids = core_affinity::get_core_ids().expect("Failed to get core IDs");
                    for core in core_ids {
                        let success = core_affinity::set_for_current(core);
                        if success {
                            info!("VDF thread pinned to core {:?}", core);
                            break;
                        }
                    }

                    run_vdf(
                        vdf_config2,
                        global_step_number,
                        seed,
                        vdf_reset_seed,
                        new_seed_rx,
                        shutdown_rx,
                        broadcast_mining_service.clone(),
                        vdf_service.clone(),
                        atomic_global_step_number.clone(),
                    )
                });

                let actor_addresses = ActorAddresses {
                    partitions: part_actors_clone,
                    block_producer: block_producer_addr,
                    packing: packing_actor_addr,
                    mempool: mempool_addr.clone(),
                    block_index: block_index_actor_addr,
                    epoch_service: epoch_service_actor_addr,
                };

                let chunk_provider =
                    ChunkProvider::new(storage_config.clone(), storage_modules.clone());
                let arc_chunk_provider = Arc::new(chunk_provider);
                // this OnceLock is due to the cyclic chain between Reth & the Irys node, where the IrysRethProvider requires both
                // this is "safe", as the OnceLock is always set before this start function returns
                *irys_provider_1.write().unwrap() = Some(IrysRethProviderInner {
                    chunk_provider: arc_chunk_provider.clone(),
                });

                let _ = irys_node_handle_sender.send(IrysNodeCtx {
                    actor_addresses: actor_addresses.clone(),
                    reth_handle: reth_node.clone(),
                    db: irys_db.clone(),
                    config: arc_config.clone(),
                    chunk_provider: arc_chunk_provider.clone(),
                    block_index_guard: block_index_guard.clone(),
                    vdf_steps_guard: vdf_steps_guard.clone(),
                    vdf_config: vdf_config.clone(),
                    storage_config: storage_config.clone(),
                    service_senders: service_senders.clone(),
                    reth_shutdown_sender,
                    reth_thread_handle: None,
                });

                let server = run_server(ApiState {
                    mempool: mempool_addr,
                    chunk_provider: arc_chunk_provider.clone(),
                    db: irys_db,
                    reth_provider: Some(reth_node.clone()),
                    block_tree: Some(block_tree_guard.clone()),
                    block_index: Some(block_index_guard.clone()),
                    config
                })
                .await;

                let server_handle = server.handle();

                let server_stop_handle = actix_rt::spawn(async move {
                    let _ = main_actor_thread_shutdown_rx.recv().await;
                    info!("Main actor thread received shutdown signal");

                    debug!("Stopping API server");
                    server_handle.stop(true).await;
                    info!("API server stopped");
                });

                server.await.unwrap();
                server_stop_handle.await.unwrap();

                debug!("Stopping actors");
                for arbiter in arbiters {
                    arbiter.stop_and_join();
                }
                debug!("Actors stopped");

                // Send shutdown signal
                shutdown_tx.send(()).unwrap();

                debug!("Waiting for VDF thread to finish");
                // Wait for vdf thread to finish & save steps
                vdf_thread_handler.join().unwrap();

                debug!("VDF thread finished");

                reth_node
            });
            debug!("Main actor thread finished");

            reth_node_handle
        })?;

    // run reth in it's own thread w/ it's own tokio runtime
    // this is done as reth exhibits strange behaviour (notably channel dropping) when not in it's own context/when the exit future isn't been awaited
    let exec: reth::tasks::TaskExecutor = task_manager.executor();

    let reth_thread_handler = std::thread::Builder::new()
        .name("reth-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let node_config = cloned_arc.clone();

            let run_reth_until_ctrl_c_or_signal = async || {
                _ = run_to_completion_or_panic(
                    &mut task_manager,
                    run_until_ctrl_c_or_channel_message(
                        start_reth_node(
                            exec,
                            reth_chainspec,
                            node_config,
                            IrysTables::ALL,
                            reth_handle_sender,
                            irys_provider.clone(),
                            latest_block_height,
                        ),
                        reth_shutdown_receiver,
                    ),
                )
                .await;

                debug!("Sending shutdown signal to the main actor thread");
                let _ = main_actor_thread_shutdown_tx.try_send(());
                debug!("Waiting for the main actor thread to finish");
                let reth_node_handle = actor_main_thread_handle.join().unwrap();

                reth_node_handle
            };

            let reth_node = tokio_runtime.block_on(run_reth_until_ctrl_c_or_signal());

            debug!("Shutting down the rest of the reth jobs in case there are unfinished ones");
            task_manager.graceful_shutdown();

            reth_node.provider.database.db.close();
            irys_storage::reth_provider::cleanup_provider(&irys_provider);

            debug!("Reth thread finished");
        })?;

    // wait for the full handle to be send over by the actix thread
    let mut node = irys_node_handle_receiver.await?;
    node.reth_thread_handle = Some(reth_thread_handler.into());
    Ok(node)
}

async fn start_reth_node<T: HasName + HasTableType>(
    task_executor: TaskExecutor,
    chainspec: ChainSpec,
    irys_config: Arc<IrysNodeConfig>,
    tables: &[T],
    sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
    irys_provider: IrysRethProvider,
    latest_block: u64,
) -> eyre::Result<NodeExitReason> {
    let node_handle = irys_reth_node_bridge::run_node(
        Arc::new(chainspec),
        task_executor,
        irys_config,
        tables,
        irys_provider,
        latest_block,
    )
    .await?;
    debug!("Reth node started");
    sender
        .send(node_handle.node.clone())
        .expect("unable to send reth node handle");

    node_handle.node_exit_future.await
}
