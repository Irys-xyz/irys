mod partitions;
mod vdf;

use ::database::{config::get_data_dir, open_or_create_db};
use actix::{Actor, Addr, Arbiter, System};
use actors::{
    block_producer::BlockProducerActor,
    chunk_storage::ChunkStorageActor,
    mempool::{self, MempoolActor},
    mining::PartitionMiningActor,
    packing::PackingActor,
    storage_provider::StorageProvider,
    ActorAddresses,
};
use clap::Parser;
use irys_api_server::run_server;
use irys_reth_node_bridge::{
    chainspec,
    node::{RethNode, RethNodeAddOns, RethNodeHandle},
    IrysChainSpecBuilder,
};
use irys_storage::partition_provider::PartitionStorageProvider;
use irys_types::{
    app_state::AppState,
    block_production::{Partition, PartitionId},
    H256,
};
use partitions::{get_partitions_and_storage_providers, mine_partition};
use reth::{
    builder::{FullNode, NodeHandle},
    chainspec::ChainSpec,
    core::{exit::NodeExitFuture, irys_ext::NodeExitReason},
    tasks::TaskManager,
    CliContext,
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c, AsyncCliRunner};
use reth_db::{database, DatabaseEnv};
use std::{
    collections::HashMap,
    fs::canonicalize,
    future::IntoFuture,
    path::{absolute, PathBuf},
    str::FromStr,
    sync::{mpsc, Arc},
    time::Duration,
};

use futures::FutureExt;
use tokio::{
    runtime::Handle,
    sync::oneshot::{self, Sender},
    time::sleep,
};

use tracing::{debug, error, span, trace, Level};
use vdf::run_vdf;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long, default_value = "./database")]
    database: String,
}

pub struct IrysNodeConfig {
    pub sm_partition_config: Vec<(Partition, PartitionStorageProvider)>,
}

impl Default for IrysNodeConfig {
    fn default() -> Self {
        Self {
            sm_partition_config: get_partitions_and_storage_providers().unwrap(),
        }
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let (nef, _hndl) = start_irys_node(Default::default()).await?;
    nef.await?;
    Ok(())
}

pub async fn start_for_testing(config: IrysNodeConfig) -> eyre::Result<IrysNodeCtx> {
    let (_ef, hndl) = start_irys_node(config).await?;
    Ok(hndl)
}

#[derive(Debug, Clone)]
pub struct IrysNodeCtx {
    pub reth_handle: FullNode<RethNode, RethNodeAddOns>,
    pub storage_provider: Arc<StorageProvider>,
    pub actor_addresses: ActorAddresses,
    pub db: Arc<DatabaseEnv>,
}

async fn start_irys_node(
    node_config: IrysNodeConfig,
) -> eyre::Result<(NodeExitFuture, IrysNodeCtx)> {
    let (actor_addr_channel_sender, actor_addr_channel_receiver) =
        oneshot::channel::<ActorAddresses>();

    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();
    let (irys_node_handle_sender, irys_node_handle_receiver) = oneshot::channel::<IrysNodeCtx>();
    // Spawn thread and runtime for actors
    std::thread::spawn(move || {
        let rt = actix_rt::Runtime::new().unwrap();
        rt.block_on(async move {
            // thanks I hate it
            let reth_handle = reth_handle_receiver.await.unwrap();
            let arc_db = reth_handle.clone().provider.database.db;

            let mempool_actor = MempoolActor::new(arc_db.clone());
            let mempool_actor_addr = mempool_actor.start();

            let mut part_actors = Vec::new();

            let block_producer_actor =
                BlockProducerActor::new(arc_db.clone(), mempool_actor_addr.clone());
            let block_producer_addr = block_producer_actor.start();

            let mut partition_storage_providers =
                HashMap::<PartitionId, PartitionStorageProvider>::new();
            for (part, storage_provider) in node_config.sm_partition_config
            /* get_partitions_and_storage_providers().unwrap() */
            {
                // let partition_chunk_storage_actor = ChunkStorageActor::new(storage_provider);
                // let addr = partition_chunk_storage_actor.start();
                // chunk_storage_actors.push(addr.clone());
                partition_storage_providers.insert(part.id, storage_provider.clone());

                let partition_mining_actor =
                    PartitionMiningActor::new(part, block_producer_addr.clone(), storage_provider);
                part_actors.push(partition_mining_actor.start());
            }

            let storage_provider =
                Arc::new(StorageProvider::new(Some(partition_storage_providers)));

            let (new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();

            let part_actors_clone = part_actors.clone();
            std::thread::spawn(move || run_vdf(H256::random(), new_seed_rx, part_actors));

            let packing_actor_addr = PackingActor::new(Handle::current()).start();

            let actor_addresses = ActorAddresses {
                partitions: part_actors_clone,
                block_producer: block_producer_addr,
                packing: packing_actor_addr,
                mempool: mempool_actor_addr,
            };

            let _ = actor_addr_channel_sender.send(actor_addresses.clone());

            let _ = irys_node_handle_sender.send(IrysNodeCtx {
                storage_provider,
                actor_addresses: actor_addresses.clone(),
                reth_handle,
                db: arc_db.clone(),
            });

            run_server(actor_addresses).await;
        });
    });

    // reth_tracing::init_test_tracing();

    let builder = IrysChainSpecBuilder::mainnet();
    let reth_chainspec = builder.reth_builder.build();

    // std::thread::Builder::new()
    //     .stack_size(32 * 1024 * 1024)
    //     .spawn(move || {
    //         TaskManager::current()
    //         let tokio_runtime = /* Handle::current(); */ tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    //             let task_manager = TaskManager::new(tokio_runtime.handle().clone());
    //             let exec = task_manager.executor();

    //         let pb = absolute(PathBuf::from_str("./.reth").unwrap()).unwrap();

    //         tokio_runtime.block_on(run_to_completion_or_panic(
    //             &mut task_manager,
    //             run_until_ctrl_c(start_reth_node(context, reth_chainspec, reth_handle_sender)),
    //         ));

    //         // let reth_node_handle =  tokio_runtime.block_on(irys_reth_node_bridge::run_node(
    //         //     Arc::new(reth_chainspec),
    //         //     exec,
    //         //     pb,
    //         // )).unwrap();
    //         // .await?;

    //         let _ = reth_handle_sender
    //             .send(reth_node_handle)
    //             .expect("unable to send reth node handle");
    //     })?;
    // note: for some reason TaskManager::current() works, when creating a new TaskManager with Handle::current doesn't
    // ...despite it being functionally identical
    let task_manager = TaskManager::current();
    let exec = task_manager.executor();

    let pb = absolute(PathBuf::from_str("./.reth").unwrap()).unwrap();

    let reth_node_handle =
        irys_reth_node_bridge::run_node(Arc::new(reth_chainspec), exec, pb).await?;

    let nef = reth_node_handle.node_exit_future;

    let _ = reth_handle_sender
        .send(reth_node_handle.node.clone())
        .expect("unable to send reth node handle");
    // send over the reth handle, wait for the full handle
    return Ok((nef, irys_node_handle_receiver.await?));
}

// async fn start_irys_node_blocking(
//     node_config: IrysNodeConfig,
//     context: CliContext,
//     handle_channel_sender: Option<Sender<IrysNodeHandle>>,
// ) -> eyre::Result<NodeExitReason> {
//     let r1 = start_irys_node(node_config, handle_channel_sender, Some(context)).await?;
//     todo!();
//     //return r1.node_exit_future.await;
// }

// fn start_irys_node_cli(
//     config: IrysNodeConfig,
//     handle_channel_sender: Option<Sender<IrysNodeHandle>>,
// ) -> eyre::Result<()> {
//     // let builder = IrysChainSpecBuilder::mainnet();
//     // let reth_chainspec = builder.reth_builder.build();

//     // TODO @JesseTheRobot - make sure logging is initialized before we get here as this uses logging macros
//     // use the existing reth code to handle blocking & graceful shutdown

//     let AsyncCliRunner {
//         context,
//         mut task_manager,
//         tokio_runtime,
//     } = AsyncCliRunner::new()?;

//     // let tokio_runtime = Handle::current();
//     // let mut task_manager = TaskManager::new(tokio_runtime.clone());
//     // let exec = task_manager.executor();
//     // let context = CliContext {
//     //     task_executor: exec,
//     // };

//     // Executes the command until it finished or ctrl-c was fired
//     let command_res = tokio_runtime.block_on(run_to_completion_or_panic(
//         &mut task_manager,
//         run_until_ctrl_c(start_irys_node_blocking(
//             config,
//             context,
//             handle_channel_sender,
//         )),
//     ));

//     if command_res.is_err() {
//         error!(target: "reth::cli", "shutting down due to error");
//         // dbg!(command_res);
//         error!("error: {:#?}", &command_res);
//     } else {
//         debug!(target: "reth::cli", "shutting down gracefully");
//         // after the command has finished or exit signal was received we shutdown the task
//         // manager which fires the shutdown signal to all tasks spawned via the task
//         // executor and awaiting on tasks spawned with graceful shutdown
//         task_manager.graceful_shutdown_with_timeout(Duration::from_secs(5));
//     }

//     // `drop(tokio_runtime)` would block the current thread until its pools
//     // (including blocking pool) are shutdown. Since we want to exit as soon as possible, drop
//     // it on a separate thread and wait for up to 5 seconds for this operation to
//     // complete.
//     let (tx, rx) = mpsc::channel();
//     std::thread::Builder::new()
//         .name("tokio-runtime-shutdown".to_string())
//         .spawn(move || {
//             drop(tokio_runtime);
//             let _ = tx.send(());
//         })
//         .unwrap();

//     let _ = rx.recv_timeout(Duration::from_secs(5)).inspect_err(|err| {
//         debug!(target: "reth::cli", %err, "tokio runtime shutdown timed out");
//     });

//     // command_res
//     Ok(())
// }
