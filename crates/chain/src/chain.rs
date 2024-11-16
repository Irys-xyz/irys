use ::database::{tables::Tables, BlockIndex, Initialized};
use actix::Actor;
use actors::{
    block_index::BlockIndexActor, block_producer::BlockProducerActor, mempool::MempoolActor,
    mining::PartitionMiningActor, packing::PackingActor, ActorAddresses,
};
use irys_api_server::run_server;
use irys_config::IrysNodeConfig;
pub use irys_reth_node_bridge::node::{
    RethNode, RethNodeAddOns, RethNodeExitHandle, RethNodeProvider,
};
use irys_storage::{partition_provider::PartitionStorageProvider, StorageProvider};
use irys_types::{app_state::DatabaseProvider, block_production::PartitionId, H256};
use reth::{
    builder::FullNode,
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    tasks::{TaskExecutor, TaskManager},
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c};
use reth_db::{HasName, HasTableType};
use std::{
    collections::HashMap,
    sync::{mpsc, Arc, RwLock},
};

use tokio::{
    runtime::Handle,
    sync::oneshot::{self},
};

use crate::vdf::run_vdf;

pub async fn start_for_testing(config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    start_irys_node(config).await
}

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: RethNodeProvider,
    pub storage_provider: StorageProvider,
    pub actor_addresses: ActorAddresses,
    pub db: DatabaseProvider,
    pub config: Arc<IrysNodeConfig>,
}

pub async fn start_irys_node(node_config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
    let (irys_node_handle_sender, irys_node_handle_receiver) = oneshot::channel::<IrysNodeCtx>();
    let irys_genesis = node_config.chainspec_builder.genesis();
    let arc_config = Arc::new(node_config);
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())?
            .init(irys_genesis, arc_config.clone())
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

                let mempool_actor = MempoolActor::new(
                    db.clone(),
                    reth_node.task_executor.clone(),
                    node_config.mining_signer.clone(),
                );
                let mempool_actor_addr = mempool_actor.start();

                let block_index_actor = BlockIndexActor::new(block_index);
                let block_index_actor_addr = block_index_actor.start();

                let mut part_actors = Vec::new();

                let block_producer_actor = BlockProducerActor::new(
                    db.clone(),
                    mempool_actor_addr.clone(),
                    block_index_actor_addr.clone(),
                    reth_node.clone(),
                    // &block_index,
                );
                let block_producer_addr = block_producer_actor.start();

                let mut partition_storage_providers =
                    HashMap::<PartitionId, PartitionStorageProvider>::new();

                for (part, storage_provider_config) in node_config.sm_partition_config.clone() {
                    let storage_provider =
                        PartitionStorageProvider::from_config(storage_provider_config).unwrap();
                    partition_storage_providers.insert(part.id, storage_provider.clone());

                    let partition_mining_actor = PartitionMiningActor::new(
                        part,
                        db.clone(),
                        block_producer_addr.clone(),
                        storage_provider,
                        false, // do not start mining automatically
                    );
                    part_actors.push(partition_mining_actor.start());
                }

                let storage_provider = StorageProvider::new(Some(partition_storage_providers));

                let (new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();

                let part_actors_clone = part_actors.clone();
                std::thread::spawn(move || run_vdf(H256::random(), new_seed_rx, part_actors));

                let packing_actor_addr =
                    PackingActor::new(Handle::current(), storage_provider.clone()).start();

                let actor_addresses = ActorAddresses {
                    partitions: part_actors_clone,
                    block_producer: block_producer_addr,
                    packing: packing_actor_addr,
                    mempool: mempool_actor_addr,
                    block_index: block_index_actor_addr,
                };

                let _ = irys_node_handle_sender.send(IrysNodeCtx {
                    storage_provider,
                    actor_addresses: actor_addresses.clone(),
                    reth_handle: reth_node,
                    db: db.clone(),
                    config: arc_config.clone(),
                });

                run_server(actor_addresses).await;
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
                run_until_ctrl_c(start_reth_node(exec, reth_chainspec, node_config, Tables::ALL, reth_handle_sender)),
            )).unwrap();
        })?;

    // wait for the full handle to be send over by the actix thread
    return Ok(irys_node_handle_receiver.await?);
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
