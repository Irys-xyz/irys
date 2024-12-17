use ::irys_database::{tables::IrysTables, BlockIndex, Initialized};
use actix::Actor;
use irys_actors::{
    block_index::BlockIndexActor,
    block_producer::{BlockConfirmedMessage, BlockProducerActor},
    chunk_provider::ChunkProviderActor,
    chunk_storage::ChunkStorageActor,
    epoch_service::{
        EpochServiceActor, EpochServiceConfig, GetGenesisStorageModulesMessage, GetLedgersMessage,
        NewEpochMessage,
    },
    mempool::MempoolActor,
    mining::PartitionMiningActor,
    mining_broadcaster::{self, BroadcastDifficultyUpdate, MiningBroadcaster},
    packing::{PackingActor, PackingRequest},
    ActorAddresses,
};
use irys_api_server::{run_server, ApiState};
use irys_config::IrysNodeConfig;
pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};

use irys_storage::{initialize_storage_files, ChunkType, StorageModule, StorageModuleVec};
use irys_types::{
    app_state::DatabaseProvider, calculate_initial_difficulty, storage_config,
    DifficultyAdjustmentConfig, StorageConfig, H256, PACKING_SHA_1_5_S, U256,
};
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c};
use reth_db::{Database as _, HasName, HasTableType};
use std::{
    sync::{mpsc, Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, info};

use tokio::{
    runtime::Handle,
    sync::oneshot::{self},
};

use crate::vdf::{run_vdf, VDFStepsConfig};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;

pub async fn start_for_testing(config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    start_irys_node(config).await
}

pub async fn start_for_testing_default(
    name: Option<&str>,
    keep: bool,
) -> eyre::Result<IrysNodeCtx> {
    let config = IrysNodeConfig {
        base_directory: setup_tracing_and_temp_dir(name, keep).into_path(),
        ..Default::default()
    };
    start_irys_node(config).await
}

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub config: Arc<IrysNodeConfig>,
}

pub async fn start_irys_node(node_config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    info!("Using directory {:?}", &node_config.base_directory);
    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
    let (irys_node_handle_sender, irys_node_handle_receiver) = oneshot::channel::<IrysNodeCtx>();
    let mut irys_genesis = node_config.chainspec_builder.genesis();
    let arc_config = Arc::new(node_config);
    let mut difficulty_adjustment_config = DifficultyAdjustmentConfig {
        target_block_time: 1,        // 1->5 seconds
        adjustment_interval: 20,     // every X blocks
        max_adjustment_factor: 4,    // No more than 4x or 1/4th with each adjustment
        min_adjustment_factor: 0.25, // a minimum 25% adjustment threshold
        min_difficulty: U256::one(),
        max_difficulty: U256::MAX,
    };
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 400,
        num_chunks_in_recall_range: 80,
        num_partitions_in_slot: 1,
        miner_address: arc_config.mining_signer.address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: PACKING_SHA_1_5_S,
    };

    irys_genesis.diff =
        calculate_initial_difficulty(&difficulty_adjustment_config, &storage_config, 3).unwrap();

    difficulty_adjustment_config.target_block_time = 5;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    irys_genesis.timestamp = now.as_millis();
    irys_genesis.last_diff_timestamp = irys_genesis.timestamp;
    let arc_genesis = Arc::new(irys_genesis);

    let mut storage_modules: StorageModuleVec = Vec::new();
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())?
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let reth_chainspec = arc_config
        .clone()
        .chainspec_builder
        .reth_builder
        .clone()
        .build();

    let cloned_arc = arc_config.clone();

    // Spawn thread and runtime for actors
    let arc_config_copy = arc_config.clone();
    std::thread::Builder::new()
        .name("actor-main-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let rt: actix_rt::Runtime = actix_rt::Runtime::new().unwrap();
            let node_config = arc_config_copy.clone();
            rt.block_on(async move {
                // the RethNodeHandle doesn't *need* to be Arc, but it will reduce the copy cost
                let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await.unwrap()));
                let db = DatabaseProvider(reth_node.provider.database.db.clone());

                // Initialize the epoch_service actor to handle partition ledger assignments
                let config = EpochServiceConfig {
                    storage_config: storage_config.clone(),
                    ..Default::default()
                };

                let miner_address = node_config.mining_signer.address();
                let epoch_service = EpochServiceActor::new(Some(config));
                let epoch_service_actor_addr = epoch_service.start();

                // Initialize the block_index actor and tell it about the genesis block
                let block_index_actor =
                    BlockIndexActor::new(block_index.clone(), storage_config.clone());
                let block_index_actor_addr = block_index_actor.start();
                let msg = BlockConfirmedMessage(arc_genesis.clone(), Arc::new(vec![]));
                db.update_eyre(|tx| irys_database::insert_block_header(tx, &arc_genesis))
                    .unwrap();
                match block_index_actor_addr.send(msg).await {
                    Ok(_) => info!("Genesis block indexed"),
                    Err(_) => panic!("Failed to index genesis block"),
                }

                // Tell the epoch_service to initialize the ledgers
                let msg = NewEpochMessage(arc_genesis.clone());
                match epoch_service_actor_addr.send(msg).await {
                    Ok(_) => info!("Genesis Epoch tasks complete."),
                    Err(_) => panic!("Failed to perform genesis epoch tasks"),
                }

                // Retrieve ledger assignments
                let ledgers_guard = epoch_service_actor_addr
                    .send(GetLedgersMessage)
                    .await
                    .unwrap();

                {
                    let ledgers = ledgers_guard.read();
                    debug!("ledgers: {:?}", ledgers);
                }

                // Get the genesis storage modules and their assigned partitions
                let storage_module_infos = epoch_service_actor_addr
                    .send(GetGenesisStorageModulesMessage)
                    .await
                    .unwrap();

                // For Genesis we create the storage_modules and their files
                initialize_storage_files(&arc_config.storage_module_dir(), &storage_module_infos)
                    .unwrap();

                // Create a list of storage modules wrapping the storage files
                for info in storage_module_infos {
                    let arc_module = Arc::new(StorageModule::new(
                        &arc_config.storage_module_dir(),
                        &info,
                        storage_config.clone(),
                    ));
                    storage_modules.push(arc_module.clone());
                    arc_module.pack_with_zeros();
                }

                let mempool_actor = MempoolActor::new(
                    db.clone(),
                    reth_node.task_executor.clone(),
                    node_config.mining_signer.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                );

                let mempool_actor_addr = mempool_actor.start();

                let chunk_storage_actor = ChunkStorageActor::new(
                    block_index.clone(),
                    storage_config.clone(),
                    storage_modules.clone(),
                    db.clone(),
                );
                let chunk_storage_addr = chunk_storage_actor.start();

                let (new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();

                let mining_broadcaster = MiningBroadcaster::new();
                let mining_broadcaster_addr = mining_broadcaster.start();

                let block_producer_actor = BlockProducerActor::new(
                    db.clone(),
                    mempool_actor_addr.clone(),
                    chunk_storage_addr.clone(),
                    block_index_actor_addr.clone(),
                    mining_broadcaster_addr.clone(),
                    epoch_service_actor_addr.clone(),
                    reth_node.clone(),
                    difficulty_adjustment_config.clone(),
                );
                let block_producer_addr = block_producer_actor.start();

                let mut part_actors = Vec::new();

                for sm in &storage_modules {
                    let partition_mining_actor = PartitionMiningActor::new(
                        miner_address,
                        db.clone(),
                        block_producer_addr.clone().recipient(),
                        mining_broadcaster_addr.clone(),
                        sm.clone(),
                        false, // do not start mining automatically
                    );
                    part_actors.push(partition_mining_actor.start());
                }

                // Yield to let actors process their mailboxes (and subscribe to the mining_broadcaster)
                tokio::task::yield_now().await;

                // Let the partition actors know about the genesis difficulty
                mining_broadcaster_addr.do_send(BroadcastDifficultyUpdate(arc_genesis.clone()));

                let part_actors_clone = part_actors.clone();
                std::thread::spawn(move || {
                    run_vdf(
                        VDFStepsConfig::default(),
                        H256::random(),
                        new_seed_rx,
                        mining_broadcaster_addr.clone(),
                    )
                });

                let packing_actor_addr =
                    PackingActor::new(Handle::current(), reth_node.task_executor.clone(), None)
                        .start();

                // request packing for uninitialized ranges
                for sm in &storage_modules {
                    let uninitialized = sm.get_intervals(ChunkType::Uninitialized);
                    let _ = uninitialized
                        .iter()
                        .map(|interval| {
                            packing_actor_addr.do_send(PackingRequest {
                                storage_module: sm.clone(),
                                chunk_range: (*interval).into(),
                            })
                        })
                        .collect::<Vec<()>>();
                }

                let actor_addresses = ActorAddresses {
                    partitions: part_actors_clone,
                    block_producer: block_producer_addr,
                    packing: packing_actor_addr,
                    mempool: mempool_actor_addr.clone(),
                    block_index: block_index_actor_addr,
                    epoch_service: epoch_service_actor_addr,
                };

                let _ = irys_node_handle_sender.send(IrysNodeCtx {
                    actor_addresses: actor_addresses.clone(),
                    reth_handle: reth_node,
                    db: db.clone(),
                    config: arc_config.clone(),
                });

                let chunk_provider_actor = ChunkProviderActor::new(
                    storage_config.clone(),
                    storage_modules.clone(),
                    db.clone(),
                );
                let chunk_provider_addr = chunk_provider_actor.start();

                run_server(ApiState {
                    mempool: mempool_actor_addr,
                    chunk_provider: chunk_provider_addr,
                    db,
                })
                .await;
            });
        })?;

    // run reth in it's own thread w/ it's own tokio runtime
    // this is done as reth exhibits strange behaviour (notably channel dropping) when not in it's own context/when the exit future isn't been awaited

    std::thread::Builder::new().name("reth-thread".to_string())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let node_config= cloned_arc.clone();
            let tokio_runtime = /* Handle::current(); */ tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let mut task_manager = TaskManager::new(tokio_runtime.handle().clone());
            let exec: reth::tasks::TaskExecutor = task_manager.executor();

            tokio_runtime.block_on(run_to_completion_or_panic(
                &mut task_manager,
                run_until_ctrl_c(start_reth_node(exec, reth_chainspec, node_config, IrysTables::ALL, reth_handle_sender)),
            )).unwrap();
        })?;

    // wait for the full handle to be send over by the actix thread
    Ok(irys_node_handle_receiver.await?)
}

async fn start_reth_node<T: HasName + HasTableType>(
    exec: TaskExecutor,
    chainspec: ChainSpec,
    irys_config: Arc<IrysNodeConfig>,
    tables: &[T],
    sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
) -> eyre::Result<NodeExitReason> {
    let node_handle =
        irys_reth_node_bridge::run_node(Arc::new(chainspec), exec, irys_config, tables).await?;
    sender
        .send(node_handle.node.clone())
        .expect("unable to send reth node handle");
    let exit_reason = node_handle.node_exit_future.await?;
    Ok(exit_reason)
}
