use clap::{Parser, Subcommand};
use eyre::{OptionExt as _, bail};
use irys_chain::utils::load_config;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, DatabaseProvider, H256, NodeConfig};
use reth_node_core::version::default_client_version;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackTarget {
    Height(u64),
    Hash(H256),
}

pub fn parse_rollback_target(target: &str) -> eyre::Result<RollbackTarget> {
    if let Ok(height) = target.parse::<u64>() {
        return Ok(RollbackTarget::Height(height));
    }
    if let Ok(hash) = H256::from_base58_result(target) {
        return Ok(RollbackTarget::Hash(hash));
    }
    bail!(
        "Invalid target {} - could not parse as a height or a valid irys block hash",
        target
    )
}

pub fn timestamp_millis_to_secs(millis: u128) -> eyre::Result<u64> {
    let secs = millis / 1000;
    u64::try_from(secs).map_err(|_| eyre::eyre!("timestamp_secs {} overflows u64", secs))
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
            let timestamp_secs =
                timestamp_millis_to_secs(config.consensus.genesis.timestamp_millis)?;
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

            let block_index = irys_domain::BlockIndex::new(&node_config, db.clone())?;

            let latest = block_index.latest_height();
            let num = block_index.num_blocks();

            let target_height = match mode {
                RollbackMode::ToBlock { target } => match parse_rollback_target(&target)? {
                    RollbackTarget::Height(height) => {
                        if latest < height {
                            warn!(
                                "Block index is at {}, which is smaller than rollback height {}",
                                latest, &height
                            );
                            return Ok(());
                        }
                        height
                    }
                    RollbackTarget::Hash(hash) => {
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
                    }
                },
                RollbackMode::Count { count } => latest.saturating_sub(count),
            };

            let remove_count = latest.saturating_sub(target_height);

            info!(
                "Old height: {}, target height: {} - removing {} blocks",
                latest, target_height, remove_count
            );

            use irys_database::reth_db::transaction::DbTxMut as _;
            let rw_tx = db.tx_mut()?;

            for h in (target_height + 1)..=latest {
                if let Some(item) = block_index.get_item(h) {
                    info!("Removing block {}@{}", &item.block_hash, h);
                    rw_tx
                        .delete::<irys_database::tables::IrysBlockHeaders>(item.block_hash, None)?;
                }
            }

            irys_database::delete_block_index_range(&rw_tx, (target_height + 1)..=latest)?;

            use irys_database::reth_db::transaction::DbTx as _;
            rw_tx.commit()?;

            info!("Rollback complete. New tip is at height {}", target_height);
            Ok(())
        }
        Commands::Tui {
            node_urls,
            config,
            record,
        } => {
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

            let mut terminal = irys_tui::utils::terminal::init()?;

            let app_result = if record {
                let mut app = irys_tui::app::App::new(node_urls, config)?
                    .start_recording()
                    .await?;
                app.run(&mut terminal).await
            } else {
                let mut app = irys_tui::app::App::new(node_urls, config)?;
                app.run(&mut terminal).await
            };

            irys_tui::utils::terminal::restore()?;

            app_result
        }
    }
}

pub fn cli_init_reth_db(access: DatabaseEnvKind) -> eyre::Result<Arc<DatabaseEnv>> {
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

    let db_path = node_config.reth_data_dir().join("db");
    let reth_db = Arc::new(DatabaseEnv::open(
        &db_path,
        DatabaseEnvKind::RO,
        irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )?);

    let timestamp_secs = timestamp_millis_to_secs(config.consensus.genesis.timestamp_millis)?;
    let chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    let static_files_path = node_config.reth_data_dir().join("static_files");
    let static_file_provider = StaticFileProvider::read_only(static_files_path, false)?;

    // Workaround: zero-sized stub -- rocksdb feature must be disabled
    const _: () = assert!(
        std::mem::size_of::<reth_provider::providers::RocksDBProvider>() == 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(&["irys-cli", "rollback-blocks", "to-block", "42"], "42")]
    #[case(&["irys-cli", "rollback-blocks", "to-block", "0"], "0")]
    #[case(&["irys-cli", "rollback-blocks", "to-block", "18446744073709551615"], "18446744073709551615")]
    fn test_rollback_to_block_parsing(#[case] args: &[&str], #[case] expected_target: &str) {
        let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
        match cli.command {
            Commands::RollbackBlocks {
                mode: RollbackMode::ToBlock { target },
            } => {
                assert_eq!(target, expected_target);
            }
            other => panic!("expected RollbackBlocks(ToBlock), got {:?}", other),
        }
    }

    #[rstest]
    #[case(&["irys-cli", "rollback-blocks", "count", "5"], 5)]
    #[case(&["irys-cli", "rollback-blocks", "count", "0"], 0)]
    #[case(&["irys-cli", "rollback-blocks", "count", "1000000"], 1_000_000)]
    fn test_rollback_count_parsing(#[case] args: &[&str], #[case] expected_count: u64) {
        let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
        match cli.command {
            Commands::RollbackBlocks {
                mode: RollbackMode::Count { count },
            } => {
                assert_eq!(count, expected_count);
            }
            other => panic!("expected RollbackBlocks(Count), got {:?}", other),
        }
    }

    #[rstest]
    #[case(&["irys-cli", "dump-state"])]
    fn test_dump_state_parsing(#[case] args: &[&str]) {
        let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
        assert!(
            matches!(cli.command, Commands::DumpState { .. }),
            "expected DumpState, got {:?}",
            cli.command
        );
    }

    #[rstest]
    #[case(&["irys-cli", "init-state", "/tmp/state.json"], "/tmp/state.json")]
    #[case(&["irys-cli", "init-state", "relative/path"], "relative/path")]
    fn test_init_state_parsing(#[case] args: &[&str], #[case] expected_path: &str) {
        let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
        match cli.command {
            Commands::InitState { state_path } => {
                assert_eq!(state_path, PathBuf::from(expected_path));
            }
            other => panic!("expected InitState, got {:?}", other),
        }
    }

    #[rstest]
    #[case(
        &["irys-cli", "tui", "http://localhost:19080"],
        vec!["http://localhost:19080"],
        None,
        false
    )]
    #[case(
        &["irys-cli", "tui", "http://a:1", "http://b:2"],
        vec!["http://a:1", "http://b:2"],
        None,
        false
    )]
    #[case(
        &["irys-cli", "tui", "--config", "tui.toml"],
        vec![],
        Some("tui.toml"),
        false
    )]
    #[case(
        &["irys-cli", "tui", "--record", "http://localhost:19080"],
        vec!["http://localhost:19080"],
        None,
        true
    )]
    fn test_tui_parsing(
        #[case] args: &[&str],
        #[case] expected_urls: Vec<&str>,
        #[case] expected_config: Option<&str>,
        #[case] expected_record: bool,
    ) {
        let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
        match cli.command {
            Commands::Tui {
                node_urls,
                config,
                record,
            } => {
                let expected: Vec<String> = expected_urls.into_iter().map(String::from).collect();
                assert_eq!(node_urls, expected);
                assert_eq!(config.as_deref(), expected_config);
                assert_eq!(record, expected_record);
            }
            other => panic!("expected Tui, got {:?}", other),
        }
    }

    #[rstest]
    #[case(&["irys-cli"])]
    #[case(&["irys-cli", "nonexistent-command"])]
    #[case(&["irys-cli", "rollback-blocks"])]
    #[case(&["irys-cli", "rollback-blocks", "count"])]
    #[case(&["irys-cli", "init-state"])]
    fn test_cli_rejects_invalid_args(#[case] args: &[&str]) {
        assert!(
            IrysCli::try_parse_from(args).is_err(),
            "expected parse failure for {:?}",
            args
        );
    }

    #[rstest]
    #[case("0", RollbackTarget::Height(0))]
    #[case("42", RollbackTarget::Height(42))]
    #[case("18446744073709551615", RollbackTarget::Height(u64::MAX))]
    fn test_parse_rollback_target_height(#[case] input: &str, #[case] expected: RollbackTarget) {
        let result = parse_rollback_target(input).expect("should parse as height");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_parse_rollback_target_hash() {
        let hash = H256::random();
        let encoded = hash.to_string();
        let result = parse_rollback_target(&encoded).expect("should parse as hash");
        assert_eq!(result, RollbackTarget::Hash(hash));
    }

    #[rstest]
    #[case("not-a-number-or-hash")]
    #[case("0x1234")]
    #[case("-1")]
    #[case("hello world with spaces")]
    fn test_parse_rollback_target_invalid(#[case] input: &str) {
        assert!(
            parse_rollback_target(input).is_err(),
            "expected error for {:?}",
            input
        );
    }

    #[rstest]
    #[case(0_u128, 0)]
    #[case(1000, 1)]
    #[case(1500, 1)]
    #[case(999, 0)]
    #[case(60_000, 60)]
    #[case(1_763_749_823_171, 1_763_749_823)]
    fn test_timestamp_millis_to_secs(#[case] millis: u128, #[case] expected_secs: u64) {
        let result = timestamp_millis_to_secs(millis).expect("should convert");
        assert_eq!(result, expected_secs);
    }

    #[test]
    fn test_timestamp_millis_to_secs_overflow() {
        let overflow_value: u128 = (u128::from(u64::MAX) + 1) * 1000;
        assert!(
            timestamp_millis_to_secs(overflow_value).is_err(),
            "should reject values whose seconds overflow u64"
        );
    }

    #[test]
    fn test_timestamp_millis_to_secs_max_u64() {
        let result = timestamp_millis_to_secs(u128::from(u64::MAX));
        assert!(result.is_ok(), "u64::MAX in millis should be convertible");
    }

    mod proptest_fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parse_rollback_target_never_panics(s in "\\PC{0,100}") {
                let _ = parse_rollback_target(&s);
            }

            #[test]
            fn timestamp_millis_to_secs_valid_range(millis in 0_u128..=u128::from(u64::MAX) * 1000) {
                let result = timestamp_millis_to_secs(millis);
                prop_assert!(result.is_ok());
                let secs = result.unwrap();
                prop_assert_eq!(secs, u64::try_from(millis / 1000).unwrap());
            }
        }
    }
}
