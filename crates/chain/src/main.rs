mod block_producer;
mod config;
mod database;
mod partitions;
mod tables;
mod vdf;

use actix::Actor;
use block_producer::BlockProducerActor;
use clap::Parser;
use config::get_data_dir;
use database::open_or_create_db;
use irys_types::{app_state::AppState, H256};
use partitions::{get_partitions, mine_partition, PartitionMiningActor};
use reth::{core::irys_ext::NodeExitReason, CliContext};
use reth_cli_runner::{run_to_completion_or_panic, run_until_ctrl_c, AsyncCliRunner};
use std::{
    str::FromStr,
    sync::{mpsc, Arc},
    time::Duration,
};
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
    // use the existing reth code to handle blocking & graceful shutdown
    let AsyncCliRunner {
        context,
        mut task_manager,
        tokio_runtime,
    } = AsyncCliRunner::new()?;

    // Executes the command until it finished or ctrl-c was fired
    let command_res = tokio_runtime.block_on(run_to_completion_or_panic(
        &mut task_manager,
        run_until_ctrl_c(main2(context)),
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

async fn main2(ctx: CliContext) -> eyre::Result<NodeExitReason> {
    let args = Args::parse();

    let db_path = get_data_dir();

    let db = open_or_create_db(&args.database)?;

    let block_producer_actor = BlockProducerActor {};

    let block_producer_addr = block_producer_actor.start();

    let mut part_actors = Vec::new();

    for part in get_partitions() {
        let partition_mining_actor = PartitionMiningActor::new(part, block_producer_addr.clone());
        part_actors.push(partition_mining_actor.start());
    }

    let (new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();

    std::thread::spawn(move || run_vdf(H256::random(), new_seed_rx, part_actors));

    let app_state = AppState {};

    let global_app_state = Arc::new(app_state);

    let node_handle = irys_reth_node_bridge::run_node(new_seed_tx, ctx.task_executor).await?;

    Ok(NodeExitReason::Normal)
}
