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
    ActorAddresses,
};
use clap::Parser;
use irys_api_server::run_server;
use irys_reth_node_bridge::{
    chainspec,
    node::{RethNode, RethNodeAddOns, RethNodeHandle},
    IrysChainSpecBuilder,
};
use irys_types::{app_state::AppState, H256};
use partitions::{get_partitions_and_storage_providers, mine_partition};
use reth::{
    builder::{FullNode, NodeHandle},
    chainspec::ChainSpec,
    core::irys_ext::NodeExitReason,
    CliContext,
};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c, AsyncCliRunner};
use reth_db::database;
use std::{
    fs::canonicalize,
    path::{absolute, PathBuf},
    str::FromStr,
    sync::{mpsc, Arc},
    time::Duration,
};

use tokio::{runtime::Handle, sync::oneshot};

use vdf::run_vdf;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long, default_value = "./database")]
    database: String,
}

use tracing::{debug, error, trace};

fn main() -> eyre::Result<()> {
    let (actor_addr_channel_sender, actor_addr_channel_receiver) =
        oneshot::channel::<ActorAddresses>();

    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();

    // Spawn thread and runtime for actors
    std::thread::spawn(move || {
        let rt = actix_rt::Runtime::new().unwrap();
        rt.block_on(async move {
            // thanks I hate it
            let reth_handle = reth_handle_receiver.await.unwrap();
            let arc_db = reth_handle.provider.database.db;

            let mempool_actor = MempoolActor::new(arc_db.clone());
            let mempool_actor_addr = mempool_actor.start();

            let block_producer_actor =
                BlockProducerActor::new(arc_db.clone(), mempool_actor_addr.clone());
            let block_producer_addr = block_producer_actor.start();

            let mut part_actors = Vec::new();
            let mut chunk_storage_actors = Vec::new();

            for (part, storage_provider) in get_partitions_and_storage_providers().unwrap() {
                let partition_chunk_storage_actor = ChunkStorageActor::new(storage_provider);
                let addr = partition_chunk_storage_actor.start();
                chunk_storage_actors.push(addr.clone());

                let partition_mining_actor =
                    PartitionMiningActor::new(part, block_producer_addr.clone(), addr);
                part_actors.push(partition_mining_actor.start());
            }

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

            run_server(actor_addresses).await;
        });
    });

    // let actor_addresses = actor_addr_channel_receiver.blocking_recv().unwrap();

    let builder = IrysChainSpecBuilder::mainnet();
    let reth_chainspec = builder.reth_builder.build();

    // TODO @JesseTheRobot - make sure logging is initialized before we get here as this uses logging macros
    // use the existing reth code to handle blocking & graceful shutdown
    let AsyncCliRunner {
        context,
        mut task_manager,
        tokio_runtime,
    } = AsyncCliRunner::new()?;
    // Executes the command until it finished or ctrl-c was fired
    let command_res = tokio_runtime.block_on(run_to_completion_or_panic(
        &mut task_manager,
        run_until_ctrl_c(start_reth_node(context, reth_chainspec, reth_handle_sender)),
    ));

    if command_res.is_err() {
        error!(target: "reth::cli", "shutting down due to error");
    } else {
        debug!(target: "reth::cli", "shutting down gracefully");
        // after the command has finished or exit signal was received we shutdown the task
        // manager which fires the shutdown signal to all tasks spawned via the task
        // executor and awaiting on tasks spawned with graceful shutdown
        task_manager.graceful_shutdown_with_timeout(Duration::from_secs(5));
    }

    // `drop(tokio_runtime)` would block the current thread until its pools
    // (including blocking pool) are shutdown. Since we want to exit as soon as possible, drop
    // it on a separate thread and wait for up to 5 seconds for this operation to
    // complete.
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("tokio-runtime-shutdown".to_string())
        .spawn(move || {
            drop(tokio_runtime);
            let _ = tx.send(());
        })
        .unwrap();

    let _ = rx.recv_timeout(Duration::from_secs(5)).inspect_err(|err| {
        debug!(target: "reth::cli", %err, "tokio runtime shutdown timed out");
    });

    // command_res
    Ok(())
}

async fn start_reth_node(
    ctx: CliContext,
    chainspec: ChainSpec,
    sender: oneshot::Sender<FullNode<RethNode, RethNodeAddOns>>,
) -> eyre::Result<NodeExitReason> {
    // let _ = tokio::signal::ctrl_c().await;
    // Ok(NodeExitReason::Normal)
    let pb = absolute(PathBuf::from_str("./.reth").unwrap()).unwrap();
    let node_handle =
        irys_reth_node_bridge::run_node(Arc::new(chainspec), ctx.task_executor, pb).await?;
    let r = sender
        .send(node_handle.node.clone())
        .expect("unable to send reth node handle");
    dbg!(r);
    let exit_reason = node_handle.node_exit_future.await?;
    Ok(exit_reason)
}
