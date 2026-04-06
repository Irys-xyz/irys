use crate::cli_args::timestamp_millis_to_secs;
use eyre::bail;
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, NodeConfig};
use reth_node_core::version::default_client_version;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
use std::{path::PathBuf, sync::Arc};

use irys_database::reth_db::{DatabaseEnv, DatabaseEnvKind};

pub(crate) fn import_genesis_to_db(genesis_dir: &PathBuf, config: &Config) -> eyre::Result<()> {
    use irys_chain::genesis_utilities::{
        load_genesis_block_from_disk, load_genesis_commitments_from_disk,
        save_genesis_block_to_disk, save_genesis_commitments_to_disk,
    };
    use irys_database::database;
    use irys_database::reth_db::{Database as _, transaction::DbTx as _};
    use irys_domain::BlockIndex;
    use irys_types::{BlockBody, DatabaseProvider, SealedBlock};

    let genesis_block = load_genesis_block_from_disk(genesis_dir)
        .map_err(|e| eyre::eyre!("Failed to load genesis block from {:?}: {e}", genesis_dir))?;
    let commitments = load_genesis_commitments_from_disk(genesis_dir).map_err(|e| {
        eyre::eyre!(
            "Failed to load genesis commitments from {:?}: {e}",
            genesis_dir
        )
    })?;

    if !genesis_block.is_signature_valid() {
        bail!(
            "Genesis block signature is invalid for hash {}",
            genesis_block.block_hash
        );
    }

    let db_env = cli_init_irys_db(DatabaseEnvKind::RW)?;
    let db = DatabaseProvider(db_env);
    let block_index = BlockIndex::new(&config.node_config, db.clone())?;

    if block_index.num_blocks() > 0 {
        bail!(
            "Refusing to import genesis: block index already has {} block(s). \
Use an empty/reset database before importing.",
            block_index.num_blocks()
        );
    }

    let genesis_body = BlockBody {
        block_hash: genesis_block.block_hash,
        data_transactions: vec![],
        commitment_transactions: commitments.clone(),
    };
    let genesis_sealed = SealedBlock::new((*genesis_block).clone(), genesis_body)?;

    let write_tx = db.tx_mut()?;
    database::insert_block_header(&write_tx, &genesis_block)?;
    for commitment_tx in &commitments {
        database::insert_commitment_tx(&write_tx, commitment_tx)?;
    }
    BlockIndex::push_block(&write_tx, &genesis_sealed, config.consensus.chunk_size)?;
    write_tx.commit()?;

    save_genesis_block_to_disk(genesis_block, &config.node_config.base_directory)
        .map_err(|e| eyre::eyre!("Failed to write genesis block to node base_directory: {e}"))?;
    save_genesis_commitments_to_disk(&commitments, &config.node_config.base_directory).map_err(
        |e| eyre::eyre!("Failed to write genesis commitments to node base_directory: {e}"),
    )?;

    Ok(())
}
/// Load the commitment transactions referenced by an epoch block's system ledger.
pub(crate) fn load_block_commitments<T: irys_database::reth_db::transaction::DbTx>(
    read_tx: &T,
    block: &irys_types::IrysBlockHeader,
) -> eyre::Result<Vec<irys_types::CommitmentTransaction>> {
    use irys_database::commitment_tx_by_txid;
    use irys_types::SystemLedger;

    let commitment_ledger = block
        .system_ledgers
        .iter()
        .find(|b| b.ledger_id == SystemLedger::Commitment);

    let Some(ledger) = commitment_ledger else {
        return Ok(vec![]);
    };

    ledger
        .tx_ids
        .iter()
        .map(|txid| {
            commitment_tx_by_txid(read_tx, txid)?
                .ok_or_else(|| eyre::eyre!("Commitment transaction not found: txid={txid}"))
        })
        .collect()
}

#[expect(dead_code)]
pub(crate) fn cli_init_reth_db(access: DatabaseEnvKind) -> eyre::Result<Arc<DatabaseEnv>> {
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
pub(crate) fn cli_init_reth_provider() -> eyre::Result<(
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

pub(crate) fn cli_init_irys_db(access: DatabaseEnvKind) -> eyre::Result<Arc<DatabaseEnv>> {
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;

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

    let db_env = match access {
        DatabaseEnvKind::RW => {
            let db = open_or_create_irys_consensus_data_db(&db_path, config.database.sync_mode)?;
            irys_database::migration::ensure_db_version_compatible(&db)?;
            db
        }
        DatabaseEnvKind::RO => DatabaseEnv::open(
            &db_path,
            access,
            irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )?,
    };

    Ok(Arc::new(db_env))
}
