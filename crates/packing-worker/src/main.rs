use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand};
use eyre::Context as _;
use irys_packing_worker::worker::start_worker;
use irys_testing_utils::initialize_tracing;
use irys_types::remote_packing::PackingWorkerConfig;
use irys_utils::listener::create_listener;
use tokio::sync::mpsc::channel;
use tracing::debug;

#[derive(Debug, Parser, Clone)]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    #[command(name = "start")]
    Start {},
    // TODO: add a `Bench` subcommand
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
        Commands::Start {} => {
            let (_tx, rx) = channel::<irys_types::ShutdownReason>(1);
            let addr: SocketAddr =
                format!("{}:{}", &config.bind_addr, &config.bind_port).parse()?;
            let listener = create_listener(addr)?;
            start_worker(config, listener, rx).await?
        }
    }

    Ok(())
}
