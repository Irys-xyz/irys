use crate::cli_args::timestamp_millis_to_secs;
use eyre::{Context as _, bail};
use irys_types::chainspec::irys_chain_spec;
use irys_types::{Config, H256, NodeConfig};
use reth_node_core::version::default_client_version;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use irys_database::reth_db::{DatabaseEnv, DatabaseEnvKind};

/// Load NodeConfig from the CONFIG env var (default "config.toml").
///
/// This is separate from `irys_chain::utils::load_config` because CLI
/// database utilities (reth provider, irys DB init) need only the NodeConfig
/// and cannot depend on the full irys_chain config loading pipeline.
fn load_node_config_from_env() -> eyre::Result<NodeConfig> {
    let config_path = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");

    let content = std::fs::read_to_string(&config_path).map_err(|e| {
        eyre::eyre!(
            "config file {:?} not found or unreadable: {e}. \
             Set CONFIG env var or create config.toml.",
            config_path
        )
    })?;
    toml::from_str::<NodeConfig>(&content)
        .map_err(|e| eyre::eyre!("failed to parse config file {:?}: {e}", config_path))
}

pub(crate) fn import_genesis_to_db(genesis_dir: &Path, config: &Config) -> eyre::Result<()> {
    use irys_chain::genesis_utilities::{
        load_genesis_block_from_disk, load_genesis_commitments_from_disk,
        save_genesis_block_to_disk, save_genesis_commitments_to_disk,
    };
    use irys_database::database;
    use irys_database::reth_db::{Database as _, transaction::DbTx as _};
    use irys_domain::BlockIndex;
    use irys_types::{BlockBody, DatabaseProvider, SealedBlock};

    let genesis_block = load_genesis_block_from_disk(genesis_dir)
        .wrap_err_with(|| format!("loading genesis block from {:?}", genesis_dir))?;
    let commitments = load_genesis_commitments_from_disk(genesis_dir)
        .wrap_err_with(|| format!("loading genesis commitments from {:?}", genesis_dir))?;

    if !genesis_block.is_signature_valid() {
        bail!(
            "Genesis block signature is invalid for hash {}",
            genesis_block.block_hash
        );
    }

    // Validate all commitment signatures before importing.
    for (i, c) in commitments.iter().enumerate() {
        use irys_types::IrysTransactionCommon as _;
        eyre::ensure!(
            c.is_signature_valid(),
            "commitment {i} (txid={}) has an invalid signature",
            c.id(),
        );
    }

    // Reject duplicate commitment txids.
    {
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in commitments.iter().enumerate() {
            eyre::ensure!(
                seen.insert(c.id()),
                "duplicate commitment txid at index {i}: {}",
                c.id(),
            );
        }
    }

    // Verify the block header's commitment ledger txids match the supplied commitments.
    {
        use irys_types::SystemLedger;
        let ledger = genesis_block
            .system_ledgers
            .iter()
            .find(|l| l.ledger_id == SystemLedger::Commitment);
        match ledger {
            Some(ledger) => {
                let ledger_ids: Vec<H256> = ledger.tx_ids.iter().copied().collect();
                let commitment_ids: Vec<H256> = commitments
                    .iter()
                    .map(irys_types::CommitmentTransaction::id)
                    .collect();
                eyre::ensure!(
                    ledger_ids.len() == commitment_ids.len(),
                    "commitment count mismatch: block header has {} txids, \
                     but {} commitments were supplied",
                    ledger_ids.len(),
                    commitment_ids.len(),
                );
                for (i, (ledger_id, commit_id)) in
                    ledger_ids.iter().zip(commitment_ids.iter()).enumerate()
                {
                    eyre::ensure!(
                        *ledger_id == *commit_id,
                        "commitment txid mismatch at index {i}: \
                         block header has {ledger_id}, supplied commitment has {commit_id}",
                    );
                }
            }
            None => bail!("genesis block has no commitment ledger in system_ledgers"),
        }
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
        .wrap_err("writing genesis block to node base_directory")?;
    save_genesis_commitments_to_disk(&commitments, &config.node_config.base_directory)
        .wrap_err("writing genesis commitments to node base_directory")?;

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

/// Initialize reth database and provider factory for commands that need header access
pub(crate) fn cli_init_reth_provider() -> eyre::Result<(
    Arc<DatabaseEnv>,
    ProviderFactory<NodeTypesWithDBAdapter<irys_reth::IrysEthereumNode, Arc<DatabaseEnv>>>,
)> {
    let node_config = load_node_config_from_env()?;
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

    // RocksDB is disabled in this build (the `rocksdb` feature is off).
    // The stub must be zero-sized so ProviderFactory accepts it without
    // actually initializing a RocksDB instance.
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

    let config = load_node_config_from_env()?;
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
