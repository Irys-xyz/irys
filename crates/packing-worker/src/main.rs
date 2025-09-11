use std::path::PathBuf;

use clap::{command, Parser, Subcommand};
use eyre::Context as _;
use irys_testing_utils::initialize_tracing;
use tracing::debug;

use crate::{config::PackingWorkerConfig, worker::start_worker};

pub mod api;
pub mod config;
pub mod packing;
pub mod worker;

#[derive(Debug, Parser, Clone)]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    #[command(name = "start")]
    Start {},
    #[command(name = "bench")]
    Bench {},
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    initialize_tracing();

    let cli = IrysCli::try_parse()?;

    let config_path = std::env::var("CONFIG")
        .unwrap_or_else(|_| "packing-worker.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");

    debug!("Loading config from {:?}", &config_path);
    let config = std::fs::read_to_string(&config_path)
        .map(|config_file| {
            toml::from_str::<PackingWorkerConfig>(&config_file).expect("invalid config file")
        })
        .wrap_err_with(|| {
            format!("Unable to read packing worker config file from {config_path:?}")
        })?;
    match cli.command {
        Commands::Start {} => start_worker(config).await?,
        Commands::Bench {} => {
            todo!()
        }
    }

    Ok(())
}
