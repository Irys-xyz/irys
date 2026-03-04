use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, U256};
use clap::{Parser, Subcommand};
use eyre::{OptionExt as _, bail};
use irys_chain::utils::load_config;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{
    Config, ConsensusConfig, ConsensusOptions, DatabaseProvider, H256, IrysAddress, NodeConfig,
    NodeMode, PeerAddress, RethPeerInfo,
};
use k256::ecdsa::SigningKey;
use rand::rngs::OsRng;
use reth_node_core::version::default_client_version;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::SystemTime;
use std::{path::PathBuf, sync::Arc};
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_error::ErrorLayer;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _, Registry, layer::SubscriberExt as _};

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
        name = "gen-testing-node-configs",
        about = "Generate config.toml files and keypairs for testing nodes"
    )]
    GenTestingNodeConfigs {
        /// IP addresses for nodes, comma-separated
        #[arg(long, value_delimiter = ',')]
        ips: Vec<String>,
        /// Chain ID
        #[arg(long, default_value = "1271")]
        chain_id: u64,
        /// Output directory
        #[arg(long, default_value = "testing-node-configs")]
        output_dir: PathBuf,
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
                panic!(
                    "GENESIS TIMESTAMP MUST BE A CONCRETE VALUE FOR INIT STATE TO WORK! current time (ms) is: {}",
                    &SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis()
                )
            }
            info!("Using timestamp {} (secs)", &timestamp_secs);
            let chain_spec = irys_chain_spec(
                config.consensus.chain_id,
                &config.consensus.reth,
                &config.consensus.hardforks,
                timestamp_secs,
            )?;
            let runtime =
                reth_tasks::RuntimeBuilder::new(reth_tasks::RuntimeConfig::default().with_tokio(
                    reth_tasks::TokioConfig::existing_handle(tokio::runtime::Handle::current()),
                ))
                .build()?;
            init_state(node_config, chain_spec, state_path, runtime).await
        }
        Commands::RollbackBlocks { mode } => {
            let node_config: NodeConfig = load_config()?;
            let db_env = cli_init_irys_db(DatabaseEnvKind::RW)?;
            let db = DatabaseProvider(db_env);

            // Construct BlockIndex backed by DB (triggers migration if needed)
            let block_index = irys_domain::BlockIndex::new(&node_config, db.clone())?;

            let latest = block_index.latest_height();
            let num = block_index.num_blocks();

            let target_height = match mode {
                RollbackMode::ToBlock { target } => {
                    if let Ok(height) = target.parse::<u64>() {
                        if latest < height {
                            warn!(
                                "Block index is at {}, which is smaller than rollback height {}",
                                latest, &height
                            );
                            return Ok(());
                        }
                        height
                    } else if let Ok(hash) = H256::from_base58_result(&target) {
                        // Search for the block hash in the index
                        let mut found_height = None;
                        for h in 0..num {
                            if let Some(item) = block_index.get_item(h)
                                && item.block_hash == hash
                            {
                                found_height = Some(h);
                                break;
                            }
                        }
                        let height = found_height.ok_or_eyre(format!(
                            "Unable to find block {} in the block index",
                            hash
                        ))?;
                        info!("Found block {} at height {}", &hash, &height);
                        height
                    } else {
                        bail!(
                            "Invalid target {} - could not parse as a height or a valid irys block hash",
                            &target
                        )
                    }
                }
                RollbackMode::Count { count } => latest.saturating_sub(count),
            };

            let remove_count = latest.saturating_sub(target_height);

            info!(
                "Old height: {}, target height: {} - removing {} blocks",
                latest, target_height, remove_count
            );

            // Remove block headers and block index entries in a single write transaction
            use irys_database::reth_db::transaction::DbTxMut as _;
            let rw_tx = db.tx_mut()?;

            // Remove block headers for the rolled-back range
            for h in (target_height + 1)..=latest {
                if let Some(item) = block_index.get_item(h) {
                    info!("Removing block {}@{}", &item.block_hash, h);
                    rw_tx
                        .delete::<irys_database::tables::IrysBlockHeaders>(item.block_hash, None)?;
                }
            }

            // Delete block index entries
            irys_database::delete_block_index_range(&rw_tx, (target_height + 1)..=latest)?;

            use irys_database::reth_db::transaction::DbTx as _;
            rw_tx.commit()?;

            info!("Rollback complete. New tip is at height {}", target_height);
            Ok(())
        }
        Commands::GenTestingNodeConfigs {
            ips,
            chain_id,
            output_dir,
        } => {
            if ips.is_empty() {
                bail!("At least one IP address is required");
            }

            // Generate keypairs for each node
            let keys: Vec<SigningKey> = (0..ips.len())
                .map(|_| SigningKey::random(&mut OsRng))
                .collect();

            let addresses: Vec<IrysAddress> =
                keys.iter().map(IrysAddress::from_private_key).collect();

            let genesis_address = addresses[0];

            // Build consensus config from testnet defaults
            let mut consensus = ConsensusConfig::testnet();
            consensus.chain_id = chain_id;
            consensus.genesis.miner_address = genesis_address;
            consensus.genesis.reward_address = genesis_address;
            consensus.expected_genesis_hash = Some(H256::zero());

            // Add alloc balances for all nodes
            let balance = U256::from(99_999_000_000_000_000_000_000_u128);
            let mut alloc = BTreeMap::new();
            for addr in &addresses {
                let alloy_addr: Address = (*addr).into();
                alloc.insert(
                    alloy_addr,
                    GenesisAccount {
                        balance,
                        ..Default::default()
                    },
                );
            }
            consensus.reth.alloc = alloc;

            for (i, ip) in ips.iter().enumerate() {
                // Validate IP
                let _: IpAddr = ip.parse().expect("valid IP address");

                // Build peer list: all other nodes
                let trusted_peers: Vec<PeerAddress> = ips
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, peer_ip)| {
                        let peer_ip: IpAddr = peer_ip.parse().expect("valid IP address");
                        PeerAddress {
                            gossip: SocketAddr::new(peer_ip, 9009),
                            api: SocketAddr::new(peer_ip, 8080),
                            execution: RethPeerInfo {
                                peering_tcp_addr: SocketAddr::new(peer_ip, 9010),
                                ..Default::default()
                            },
                        }
                    })
                    .collect();

                let mut node_config = NodeConfig::testnet();
                node_config.mining_key = keys[i].clone();
                node_config.reward_address = addresses[i];
                node_config.node_mode = if i == 0 {
                    NodeMode::Genesis
                } else {
                    NodeMode::Peer
                };
                node_config.trusted_peers = trusted_peers;
                node_config.consensus = ConsensusOptions::Custom(consensus.clone());
                node_config.network_defaults.public_ip = ip.clone();

                // Set gossip/http/reth ports
                node_config.gossip.public_port = 9009;
                node_config.gossip.bind_port = 9009;
                node_config.http.public_port = 8080;
                node_config.http.bind_port = 8080;
                node_config.reth.network.public_port = 9010;
                node_config.reth.network.bind_port = 9010;
                node_config.reth.network.bind_ip = Some("0.0.0.0".to_string());

                // Set bind_ip to 0.0.0.0 for external accessibility
                node_config.gossip.bind_ip = Some("0.0.0.0".to_string());
                node_config.http.bind_ip = Some("0.0.0.0".to_string());

                // Set public IPs
                node_config.gossip.public_ip = Some(ip.clone());
                node_config.http.public_ip = Some(ip.clone());
                node_config.reth.network.public_ip = Some(ip.clone());

                // Whitelist all addresses for staking/pledging on genesis node
                if i == 0 {
                    node_config.initial_stake_and_pledge_whitelist = addresses.clone();
                }

                let node_dir = output_dir.join(format!("node-{}", i + 1));
                std::fs::create_dir_all(&node_dir)?;

                let config_toml = toml::to_string_pretty(&node_config)?;
                let config_path = node_dir.join("config.toml");
                std::fs::write(&config_path, config_toml)?;

                info!(
                    "Node {} ({}): config written to {}",
                    i + 1,
                    ip,
                    config_path.display()
                );
            }

            // Print summary table
            println!("\n{:<6} {:<20} {:<66} Address", "Node", "IP", "Mining Key");
            println!("{}", "-".repeat(140));
            for (i, ip) in ips.iter().enumerate() {
                let key_hex = hex::encode(keys[i].to_bytes());
                let mode = if i == 0 { "Genesis" } else { "Peer" };
                println!(
                    "{:<6} {:<20} {} {} ({})",
                    i + 1,
                    ip,
                    key_hex,
                    addresses[i],
                    mode
                );
            }

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
    // No-op stub — we don't enable the `rocksdb` feature, so this compiles to a unit struct
    // that ignores the path entirely (no filesystem access). Required by ProviderFactory::new.
    const _: () = assert!(
        size_of::<reth_provider::providers::RocksDBProvider>() == 0,
        "RocksDBProvider must be the zero-sized stub (rocksdb feature must be disabled)"
    );
    let rocksdb_provider = reth_provider::providers::RocksDBProvider::new(&db_path)?;
    let runtime = reth_tasks::RuntimeBuilder::new(reth_tasks::RuntimeConfig::default().with_tokio(
        reth_tasks::TokioConfig::existing_handle(tokio::runtime::Handle::current()),
    ))
    .build()?;
    let provider_factory = ProviderFactory::new(
        reth_db.clone(),
        chain_spec,
        static_file_provider,
        rocksdb_provider,
        runtime,
    )?;

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
