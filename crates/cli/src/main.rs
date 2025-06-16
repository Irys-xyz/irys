use clap::{command, Parser, Subcommand};
use irys_chain::utils::load_config;
use irys_config::chain::chainspec::IrysChainSpecBuilder;
use irys_database::reth_db::{DatabaseEnv, DatabaseEnvKind};
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::{Config, NodeConfig};
use reth_node_core::version::default_client_version;
use std::{path::PathBuf, sync::Arc};
use tracing::level_filters::LevelFilter;
use tracing_error::ErrorLayer;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer as _, Registry};

#[derive(Debug, Parser, Clone)]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    #[command(name = "backup-accounts")]
    BackupAccounts {},
    #[command(name = "initialize-genesis")]
    InitializeGenesis { state_path: PathBuf },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let subscriber = Registry::default();
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stdout);

    let subscriber = subscriber
        .with(filter)
        .with(ErrorLayer::default())
        .with(output_layer.boxed());

    subscriber.init();

    color_eyre::install().expect("color eyre could not be installed");

    let args = IrysCli::parse();

    let node_config: NodeConfig = load_config()?;

    match args.command {
        Commands::BackupAccounts { .. } => {
            dump_state(cli_init_db()?, "./".into())?;
            Ok(())
        }
        Commands::InitializeGenesis { state_path } => {
            let (chain_spec, _) =
                IrysChainSpecBuilder::from_config(&Config::new(node_config.clone())).build();

            init_state(node_config, chain_spec.into(), state_path).await
        }
    }
}

pub fn cli_init_db() -> eyre::Result<Arc<DatabaseEnv>> {
    // load the config
    let config = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");
    let config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });

    // open the Reth database
    let db_path = config.reth_data_dir().join("db");

    let reth_db = Arc::new(DatabaseEnv::open(
        &db_path,
        DatabaseEnvKind::RO,
        irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )?);

    Ok(reth_db)
}
