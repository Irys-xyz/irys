use crate::cli_args::{
    Commands, IrysCli, RollbackMode, RollbackTarget, parse_rollback_target,
    timestamp_millis_to_secs,
};
use crate::db_utils::{cli_init_irys_db, cli_init_reth_provider, import_genesis_to_db};
use crate::snapshot_output::{
    cli_compare_output, cli_config, cli_snapshot_output, replay_current_network_snapshot,
    snapshot_from_genesis_dir,
};

use eyre::{OptionExt as _, bail};
use irys_chain::utils::load_config;
use irys_database::reth_db::Database as _;
use irys_database::reth_db::DatabaseEnvKind;
use irys_reth_node_bridge::dump::dump_state;
use irys_reth_node_bridge::genesis::init_state;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, DatabaseProvider, NodeConfig};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{info, warn};
use zeroize::Zeroizing;

fn signing_key_from_hex(hex_str: &str) -> eyre::Result<k256::ecdsa::SigningKey> {
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
    key_file: Option<std::path::PathBuf>,
) -> eyre::Result<k256::ecdsa::SigningKey> {
    // 1. Direct hex value (from --signing-key / --key or IRYS_SIGNING_KEY)
    if let Some(hex_str) = explicit_hex {
        return signing_key_from_hex(&hex_str);
    }

    // 2. Read hex from a file (--signing-key-file / --key-file or IRYS_SIGNING_KEY_FILE)
    if let Some(path) = key_file {
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| eyre::eyre!("Failed to read key file {}: {e}", path.display()))?;
        info!("Loaded signing key from {}", path.display());
        return signing_key_from_hex(contents.trim());
    }

    // 3. Fall back to config.toml mining_key
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

pub(crate) async fn run(args: IrysCli) -> eyre::Result<()> {
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
                bail!(
                    "genesis timestamp must be a concrete value for init-state to work. \
                     Current time (ms): {}",
                    SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system clock before UNIX epoch")
                        .as_millis()
                );
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
        Commands::BuildGenesis {
            miners,
            commitments,
            signing_key,
            signing_key_file,
            output,
        } => {
            use irys_chain::genesis_builder::{
                GenesisMinerManifest, build_genesis_block_from_commitments,
                build_signed_genesis_block,
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
        Commands::GenerateMinerInfo { key, key_file } => {
            use alloy_signer::utils::secret_key_to_address;
            use irys_types::IrysAddress;

            let signing_key = resolve_signing_key(key, key_file)?;

            let evm_address = secret_key_to_address(&signing_key);
            let irys_address = IrysAddress::from(evm_address);

            println!("Irys address: {irys_address}");
            println!("EVM address:  {evm_address}");

            Ok(())
        }
        Commands::ConfiguredMinerInfo {} => {
            use alloy_signer::utils::secret_key_to_address;
            use irys_types::IrysAddress;

            let node_config: NodeConfig = load_config()?;
            let mining_key = node_config.mining_key;

            let evm_address = secret_key_to_address(&mining_key);
            let irys_address = IrysAddress::from_private_key(&mining_key);

            println!("Configured miner key address:");
            println!("Irys address: {irys_address}");
            println!("EVM address:  {evm_address}");

            Ok(())
        }
        Commands::InspectGenesis { genesis_dir } => {
            let config = cli_config()?;
            let (snapshot, commitments) =
                snapshot_from_genesis_dir(&genesis_dir, &config, "inspect-sm")?;

            let output =
                cli_snapshot_output("Genesis Block", "Commitments", &snapshot, &commitments);
            print!("{output}");

            Ok(())
        }
        Commands::ImportGenesis { genesis_dir } => {
            let config = cli_config()?;
            import_genesis_to_db(&genesis_dir, &config)?;
            let base_dir = &config.node_config.base_directory;

            info!("Genesis imported from {}", genesis_dir.display());
            info!(
                "  Persisted to database at {}",
                config.node_config.irys_consensus_data_dir().display()
            );
            info!("  Saved to node base_directory {}", base_dir.display());

            Ok(())
        }
        Commands::DumpCommitments { output } => {
            use irys_database::reth_db::Database as _;
            let config = cli_config()?;

            let db_env = cli_init_irys_db(DatabaseEnvKind::RO)?;
            let read_tx = db_env.tx()?;

            let (snapshot, all_commitments) =
                replay_current_network_snapshot(&read_tx, &config, "dump-sm")?;
            let json = serde_json::to_string_pretty(&all_commitments)
                .map_err(|e| eyre::eyre!("Failed to serialize commitments: {e}"))?;
            std::fs::write(&output, json)
                .map_err(|e| eyre::eyre!("Failed to write {}: {e}", output.display()))?;

            info!("Commitments written to {}", output.display());

            let output = cli_snapshot_output(
                "Latest Epoch Block",
                "Exported Commitments",
                &snapshot,
                &all_commitments,
            );
            print!("{output}");

            Ok(())
        }
        Commands::CompareGenesis {
            genesis_dir,
            list_retained_partition_hashes,
        } => {
            use irys_database::reth_db::Database as _;

            let config = cli_config()?;
            let db_env = cli_init_irys_db(DatabaseEnvKind::RO)?;
            let read_tx = db_env.tx()?;

            let (current_snapshot, current_commitments) =
                replay_current_network_snapshot(&read_tx, &config, "compare-current-sm")?;
            let (target_snapshot, target_commitments) =
                snapshot_from_genesis_dir(&genesis_dir, &config, "compare-target-sm")?;

            let output = cli_compare_output(
                "Current Network",
                &current_snapshot,
                &current_commitments,
                "Target Genesis",
                &target_snapshot,
                &target_commitments,
            );
            print!("{output}");
            if list_retained_partition_hashes {
                let retained_hashes = output.retained_partition_hashes();
                println!();
                println!(
                    "Retained Partition Hashes (all nodes): {}",
                    retained_hashes.len()
                );
                for hash in retained_hashes {
                    println!("{hash}");
                }
            }

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

            let app_result = async {
                if record {
                    let mut app = irys_tui::app::App::new(node_urls, config)?
                        .start_recording()
                        .await?;
                    app.run(&mut terminal).await
                } else {
                    let mut app = irys_tui::app::App::new(node_urls, config)?;
                    app.run(&mut terminal).await
                }
            }
            .await;

            irys_tui::utils::terminal::restore()?;

            app_result
        }
    }
}
