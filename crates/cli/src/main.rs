use clap::{command, Parser, Subcommand};
use eyre::{bail, OptionExt as _};
use irys_chain::utils::load_config;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, IrysPeerId, NodeConfig, H256};
use reth_node_core::version::default_client_version;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{providers::StaticFileProvider, ProviderFactory};
use std::time::SystemTime;
use std::{path::PathBuf, sync::Arc};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt as _;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_error::ErrorLayer;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer as _, Registry};

#[derive(Debug, Clone, Parser)]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    #[command(name = "dump-state")]
    DumpState {},
    #[command(name = "init-state")]
    InitState { state_path: PathBuf },
    #[command(name = "rollback-blocks")]
    RollbackBlocks {
        #[command(subcommand)]
        mode: RollbackMode,
    },
    #[command(name = "tui", about = "Launch the Irys cluster monitoring TUI")]
    Tui {
        /// Node URLs to connect to
        #[arg(value_name = "NODE_URLS")]
        node_urls: Vec<String>,

        /// Configuration file path (contains both TUI settings and nodes list)
        #[arg(short, long)]
        config: Option<String>,

        /// Record node info to SQLite database (irys-tui-records.db)
        #[arg(short, long)]
        record: bool,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum RollbackMode {
    #[command(
        name = "to-block",
        about = "Rollback to a specific block by height or hash. the provided height/hash will be the new tip of the block index"
    )]
    ToBlock {
        #[arg(help = "Block height (number) or block hash")]
        target: String,
    },
    #[command(name = "count", about = "Rollback a specific number of blocks")]
    Count {
        #[arg(help = "Number of blocks to rollback")]
        count: u64,
    },
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

    match args.command {
        Commands::DumpState { .. } => {
            let (reth_db, provider_factory) = cli_init_reth_provider()?;
            dump_state(reth_db, &provider_factory, "./".into())?;
            Ok(())
        }
        Commands::InitState { state_path } => {
            let node_config: NodeConfig = load_config()?;
            let config = Config::new(node_config.clone(), IrysPeerId::random());
            // Convert timestamp from millis to seconds for reth
            let timestamp_secs =
                std::time::Duration::from_millis(config.consensus.genesis.timestamp_millis as u64)
                    .as_secs();
            if timestamp_secs == 0 {
                panic!("GENESIS TIMESTAMP MUST BE A CONCRETE VALUE FOR INIT STATE TO WORK! current time (ms) is: {}", &SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis())
            }
            info!("Using timestamp {} (secs)", &timestamp_secs);
            let chain_spec = irys_chain_spec(
                config.consensus.chain_id,
                &config.consensus.reth,
                &config.consensus.hardforks,
                timestamp_secs,
            )?;
            init_state(node_config, chain_spec, state_path).await
        }
        Commands::RollbackBlocks { mode } => {
            let node_config: NodeConfig = load_config()?;
            let db = cli_init_irys_db(DatabaseEnvKind::RW)?;

            let block_index = irys_domain::BlockIndex::new(&node_config).await?;

            let (retained, removed) = {
                let count = match mode {
                    RollbackMode::ToBlock { target } => {
                        if let Ok(height) = target.parse::<u64>() {
                            if block_index.latest_height() < height {
                                warn!("Block index is at {}, which is smaller than rollback height {}", &block_index.latest_height(), &height);
                                return Ok(());
                            }
                            block_index.latest_height().saturating_sub(height)
                        } else if let Ok(hash) = H256::from_base58_result(&target) {
                            let idx = block_index
                                .items
                                .iter()
                                .position(|itm| itm.block_hash == hash)
                                .ok_or_eyre(format!(
                                    "Unable to find block {} in the block index",
                                    hash
                                ))?;
                            // the idx is the height
                            info!("Found block {} at height {}", &hash, &idx);
                            block_index.latest_height().saturating_sub(idx as u64)
                        } else {
                            bail!("Invalid target {} - could not parse as a height or a valid irys block hash", &target)
                        }
                    }
                    RollbackMode::Count { count } => count,
                };

                &block_index.items.split_at(
                    block_index
                        .items
                        .len()
                        .saturating_sub(count.try_into().unwrap()),
                )
            };

            info!(
                "Old len: {}, new {} - retaining <{}, removing {} -> {} ",
                block_index.items.len(),
                retained.len(),
                retained.len(),
                retained.len(),
                retained.len() + removed.len()
            );

            // remove every block in `removed` from the database
            use irys_database::reth_db::transaction::DbTxMut as _;
            let rw_tx = db.tx_mut()?;

            for itm in removed.iter() {
                let hdr = rw_tx
                    .get::<irys_database::tables::IrysBlockHeaders>(itm.block_hash)?
                    .unwrap();
                info!("Removing {}@{}", &hdr.block_hash, &hdr.height);
                rw_tx.delete::<irys_database::tables::IrysBlockHeaders>(itm.block_hash, None)?;
            }

            std::fs::copy(
                block_index.block_index_file.clone(),
                node_config.block_index_dir().join("index.dat.bak"),
            )?;

            let mut f = OpenOptions::new()
                .truncate(true)
                .write(true)
                .open(block_index.block_index_file)
                .await?;
            // TODO: update this so it a.) removes other data from the DB, and 2.) removes entries more efficiently (we know the size of each entry ahead of time)
            for item in retained.iter() {
                f.write_all(&item.to_bytes()).await?
            }
            f.sync_all().await?;

            use irys_database::reth_db::transaction::DbTx as _;
            rw_tx.commit()?;

            Ok(())
        }
        Commands::Tui {
            node_urls,
            config,
            record,
        } => {
            // Check if we have any node configuration
            if node_urls.is_empty() && config.is_none() {
                eprintln!("Error: No nodes specified.");
                eprintln!("\nYou must provide nodes via one of the following methods:");
                eprintln!(
                    "  1. Command line arguments: irys-cli tui http://node1:port http://node2:port"
                );
                eprintln!("  2. Config file: irys-cli tui --config tui.toml");
                eprintln!("\nExample:");
                eprintln!("  irys-cli tui http://localhost:19080 http://localhost:19081");
                eprintln!("  irys-cli tui --config tui.toml");
                std::process::exit(1);
            }

            // Initialize terminal
            let mut terminal = irys_tui::utils::terminal::init()?;

            // Create and run the TUI app with optional recording
            let app_result = if record {
                let mut app = irys_tui::app::App::new(node_urls, config)?
                    .start_recording()
                    .await?;
                app.run(&mut terminal).await
            } else {
                let mut app = irys_tui::app::App::new(node_urls, config)?;
                app.run(&mut terminal).await
            };

            // Restore terminal on exit
            irys_tui::utils::terminal::restore()?;

            app_result
        }
    }
}

pub fn cli_init_reth_db(access: DatabaseEnvKind) -> eyre::Result<Arc<DatabaseEnv>> {
    // load the config
    let config = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");
    let config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|err| {
            tracing::warn!(
                custom.error = ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });

    // open the Reth database
    let db_path = config.reth_data_dir().join("db");

    let reth_db = Arc::new(DatabaseEnv::open(
        &db_path,
        access,
        irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )?);

    Ok(reth_db)
}

/// Initialize reth database and provider factory for commands that need header access
pub fn cli_init_reth_provider() -> eyre::Result<(
    Arc<DatabaseEnv>,
    ProviderFactory<NodeTypesWithDBAdapter<irys_reth::IrysEthereumNode, Arc<DatabaseEnv>>>,
)> {
    // load the config
    let config = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");
    let node_config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|err| {
            tracing::warn!(
                custom.error = ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });
    let config = Config::new(node_config.clone(), IrysPeerId::random());

    // open the Reth database
    let db_path = node_config.reth_data_dir().join("db");
    let reth_db = Arc::new(DatabaseEnv::open(
        &db_path,
        DatabaseEnvKind::RO,
        irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )?);

    // Create chain spec for the provider factory
    // Convert timestamp from millis to seconds for reth
    let timestamp_secs =
        std::time::Duration::from_millis(config.consensus.genesis.timestamp_millis as u64)
            .as_secs();
    let chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    // Create static file provider for reading headers
    let static_files_path = node_config.reth_data_dir().join("static_files");
    let static_file_provider = StaticFileProvider::read_only(static_files_path, false)?;

    // Create provider factory
    let provider_factory = ProviderFactory::new(reth_db.clone(), chain_spec, static_file_provider)?;

    Ok((reth_db, provider_factory))
}

pub fn cli_init_irys_db(access: DatabaseEnvKind) -> eyre::Result<Arc<DatabaseEnv>> {
    // load the config
    let config = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");
    let config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|err| {
            tracing::warn!(
                custom.error = ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });

    // open the Irys database
    let db_path = config.irys_consensus_data_dir();

    let reth_db = Arc::new(DatabaseEnv::open(
        &db_path,
        access,
        irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )?);

    Ok(reth_db)
}
