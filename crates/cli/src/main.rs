use clap::{command, Parser, Subcommand};
use eyre::{bail, OptionExt as _};
use irys_chain::utils::load_config;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, NodeConfig, H256};
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
use zeroize::Zeroizing;

fn signing_key_from_hex(hex_str: Zeroizing<String>) -> eyre::Result<k256::ecdsa::SigningKey> {
    let key_bytes = Zeroizing::new(
        hex::decode(hex_str.trim_start_matches("0x"))
            .map_err(|e| eyre::eyre!("Invalid hex for signing key: {e}"))?,
    );
    k256::ecdsa::SigningKey::from_slice(&key_bytes)
        .map_err(|e| eyre::eyre!("Invalid signing key: {e}"))
}

/// Resolve a signing key from, in order:
/// 1. Explicit hex string (CLI arg or `IRYS_SIGNING_KEY` env var)
/// 2. Key file path (CLI arg or `IRYS_SIGNING_KEY_FILE` env var)
/// 3. `mining_key` in config.toml
fn resolve_signing_key(
    explicit_hex: Option<String>,
    key_file: Option<PathBuf>,
) -> eyre::Result<k256::ecdsa::SigningKey> {
    if let Some(hex_str) = explicit_hex {
        return signing_key_from_hex(Zeroizing::new(hex_str));
    }

    if let Some(path) = key_file {
        let contents = Zeroizing::new(
            std::fs::read_to_string(&path)
                .map_err(|e| eyre::eyre!("Failed to read key file {}: {e}", path.display()))?,
        );
        info!("Loaded signing key from {}", path.display());
        return signing_key_from_hex(Zeroizing::new(contents.trim().to_owned()));
    }

    match load_config() {
        Ok(node_config) => {
            info!("Using mining_key from config.toml as signing key");
            Ok(node_config.mining_key)
        }
        Err(e) => {
            bail!(
                "No signing key provided (config.toml error: {e}). Supply one via:\n  \
                 --signing-key <hex>\n  \
                 IRYS_SIGNING_KEY env var\n  \
                 --signing-key-file <path> / IRYS_SIGNING_KEY_FILE env var\n  \
                 mining_key in config.toml"
            )
        }
    }
}

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
    #[command(
        name = "build-genesis",
        about = "Build a signed genesis block from miner keys or existing commitments",
        group = clap::ArgGroup::new("genesis-source").required(true).args(&["miners", "commitments"]),
    )]
    BuildGenesis {
        /// Path to genesis_miners.toml containing miner keys and pledge counts.
        /// Mutually exclusive with --commitments.
        #[arg(long, conflicts_with = "commitments")]
        miners: Option<PathBuf>,

        /// Path to a JSON file of pre-signed CommitmentTransaction objects.
        /// Requires --signing-key. Mutually exclusive with --miners.
        #[arg(long, conflicts_with = "miners")]
        commitments: Option<PathBuf>,

        /// Hex-encoded secp256k1 private key for signing the genesis block header.
        /// Required when using --commitments. With --miners, the first miner signs.
        ///
        /// Resolution order: --signing-key flag, IRYS_SIGNING_KEY env var,
        /// --signing-key-file / IRYS_SIGNING_KEY_FILE, then mining_key in config.toml.
        #[arg(long, env = "IRYS_SIGNING_KEY")]
        signing_key: Option<String>,

        /// Path to a file containing the hex-encoded signing key.
        /// The file should contain only the key (whitespace is trimmed).
        #[arg(long, env = "IRYS_SIGNING_KEY_FILE")]
        signing_key_file: Option<PathBuf>,

        /// Output directory for genesis block and commitments JSON files.
        /// Defaults to current directory.
        #[arg(long, default_value = ".")]
        output: PathBuf,
    },
    #[command(
        name = "import-genesis",
        about = "Import genesis files from disk into the node database"
    )]
    ImportGenesis {
        /// Directory containing .irys_genesis.json and .irys_genesis_commitments.json
        #[arg(long, default_value = ".")]
        genesis_dir: PathBuf,
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
            let config = Config::new_with_random_peer_id(node_config.clone());
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
        Commands::BuildGenesis {
            miners,
            commitments,
            signing_key,
            signing_key_file,
            output,
        } => {
            use irys_chain::genesis_builder::{
                build_genesis_block_from_commitments, build_signed_genesis_block,
                GenesisMinerManifest,
            };
            use irys_chain::genesis_utilities::{
                save_genesis_block_to_disk, save_genesis_commitments_to_disk,
            };

            let node_config: NodeConfig = load_config()?;
            let config = Config::new_with_random_peer_id(node_config);

            let genesis_output = if let Some(miners_path) = miners {
                let manifest = GenesisMinerManifest::load(&miners_path)?;
                let miner_entries = manifest.into_entries()?;

                info!(
                    "Building genesis block with {} miner(s), {} total pledges",
                    miner_entries.len(),
                    miner_entries.iter().map(|m| m.pledge_count).sum::<u64>()
                );

                build_signed_genesis_block(&config, &miner_entries).await?
            } else if let Some(commitments_path) = commitments {
                let block_signer = resolve_signing_key(signing_key, signing_key_file)?;

                let file = std::fs::File::open(&commitments_path).map_err(|e| {
                    eyre::eyre!(
                        "Failed to read commitments file {:?}: {e}",
                        commitments_path
                    )
                })?;
                let reader = std::io::BufReader::new(file);
                let loaded_commitments: Vec<irys_types::CommitmentTransaction> =
                    serde_json::from_reader(reader)
                        .map_err(|e| eyre::eyre!("Failed to parse commitments JSON: {e}"))?;

                info!(
                    "Building genesis from {} existing commitments",
                    loaded_commitments.len()
                );

                build_genesis_block_from_commitments(&config, loaded_commitments, &block_signer)?
            } else {
                bail!("Either --miners or --commitments must be provided");
            };

            save_genesis_block_to_disk(Arc::new(genesis_output.block.clone()), &output)?;
            save_genesis_commitments_to_disk(&genesis_output.commitments, &output)?;

            info!("Genesis block written to {:?}", &output);
            info!("  Block hash: {}", genesis_output.block.block_hash);
            info!("  Commitments: {} total", genesis_output.commitments.len());
            info!(
                "  Add to peer configs: consensus.expected_genesis_hash = \"{}\"",
                genesis_output.block.block_hash
            );

            Ok(())
        }
        Commands::ImportGenesis { genesis_dir } => {
            use eyre::Context as _;
            use irys_chain::genesis_builder::validate_genesis_commitments;
            use irys_chain::genesis_utilities::{
                load_genesis_block_from_disk, load_genesis_commitments_from_disk,
                save_genesis_block_to_disk, save_genesis_commitments_to_disk,
            };
            use irys_database::database;
            use irys_database::reth_db::transaction::DbTx as _;
            use irys_database::reth_db::Database as _;
            use irys_domain::BlockIndex;
            use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
            use irys_types::{app_state::DatabaseProvider, SystemLedger};

            let node_config: NodeConfig = load_config()?;
            let config = Config::new_with_random_peer_id(node_config.clone());

            let genesis_block = load_genesis_block_from_disk(&genesis_dir)
                .wrap_err_with(|| format!("loading genesis block from {:?}", genesis_dir))?;
            let commitments = load_genesis_commitments_from_disk(&genesis_dir)
                .wrap_err_with(|| format!("loading genesis commitments from {:?}", genesis_dir))?;

            if !genesis_block.is_signature_valid() {
                bail!(
                    "Genesis block signature is invalid for hash {}",
                    genesis_block.block_hash
                );
            }

            validate_genesis_commitments(&commitments)?;

            // Verify the block header's commitment ledger txids match the supplied
            // commitments — guards against ledger/file drift.
            let ledger = genesis_block
                .system_ledgers
                .iter()
                .find(|l| l.ledger_id == SystemLedger::Commitment as u32);
            match ledger {
                Some(ledger) => {
                    eyre::ensure!(
                        ledger.tx_ids.len() == commitments.len(),
                        "commitment count mismatch: block header has {} txids, \
                         but {} commitments were supplied",
                        ledger.tx_ids.len(),
                        commitments.len(),
                    );
                    for (i, (ledger_id, commit)) in
                        ledger.tx_ids.iter().zip(commitments.iter()).enumerate()
                    {
                        eyre::ensure!(
                            *ledger_id == commit.id(),
                            "commitment txid mismatch at index {i}: \
                             block header has {ledger_id}, supplied commitment has {}",
                            commit.id(),
                        );
                    }
                }
                None => bail!("genesis block has no commitment ledger in system_ledgers"),
            }

            if let Some(expected) = config.consensus.expected_genesis_hash {
                eyre::ensure!(
                    genesis_block.block_hash == expected,
                    "genesis block hash mismatch: expected {expected}, got {}",
                    genesis_block.block_hash
                );
            }

            let irys_db_env = open_or_create_irys_consensus_data_db(
                &config.node_config.irys_consensus_data_dir(),
            )?;
            let irys_db = DatabaseProvider(Arc::new(irys_db_env));
            let mut block_index = BlockIndex::new(&config.node_config).await?;

            if block_index.num_blocks() > 0 {
                bail!(
                    "Refusing to import genesis: block index already has {} block(s). \
                     Use an empty/reset database before importing.",
                    block_index.num_blocks()
                );
            }

            let write_tx = irys_db.tx_mut()?;
            database::insert_block_header(&write_tx, &genesis_block)?;
            for commitment_tx in &commitments {
                database::insert_commitment_tx(&write_tx, commitment_tx)?;
            }
            write_tx.commit()?;

            block_index.push_block(&genesis_block, &Vec::new(), config.consensus.chunk_size)?;

            // Save to base_directory so a subsequent node start finds them in place.
            save_genesis_block_to_disk(genesis_block.clone(), &config.node_config.base_directory)
                .wrap_err(
                "writing genesis block to node base_directory — Database was already \
                     updated successfully. You may need to manually copy the genesis files \
                     to resolve this.",
            )?;
            save_genesis_commitments_to_disk(&commitments, &config.node_config.base_directory)
                .wrap_err(
                    "writing genesis commitments to node base_directory — Database was \
                     already updated successfully. You may need to manually copy the \
                     genesis files to resolve this.",
                )?;

            info!("Genesis imported from {}", genesis_dir.display());
            info!(
                "  Persisted to database at {}",
                config.node_config.irys_consensus_data_dir().display()
            );
            info!(
                "  Saved to node base_directory {}",
                config.node_config.base_directory.display()
            );

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
    let config = Config::new_with_random_peer_id(node_config.clone());

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
