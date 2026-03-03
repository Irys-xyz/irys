use irys_database::{
    commitment_tx_by_txid, open_or_create_db,
    reth_db::{Database as _, DatabaseEnv},
    tables::{IngressProofs, IrysDataTxHeaders, IrysTables},
    walk_all,
};

use irys_database::reth_db::DatabaseEnvKind;
use irys_types::NodeConfig;
use reth_node_core::version::default_client_version;
use std::{path::PathBuf, sync::Arc};
use tracing::level_filters::LevelFilter;
use tracing_error::ErrorLayer;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _, Registry, layer::SubscriberExt as _};

pub fn load_db() -> DatabaseEnv {
    let path = "/workspaces/irys-rs/.irys/1/reth/db";
    open_or_create_db(path, IrysTables::ALL, None).unwrap()
}

fn _promotion_debug() -> eyre::Result<()> {
    let db = load_db();
    let read_tx = db.tx()?;
    let ingress_proofs = walk_all::<IngressProofs, _>(&read_tx)?;
    dbg!(ingress_proofs);
    let storage_transactions = walk_all::<IrysDataTxHeaders, _>(&read_tx)?;
    dbg!(storage_transactions);

    Ok(())
}

fn _check_db_for_commitments() -> eyre::Result<()> {
    use irys_database::reth_db::Database as _;
    use irys_database::reth_db::cursor::DbCursorRO as _;
    use irys_database::reth_db::transaction::DbTx as _;

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

    let db = {
        let config = std::env::var("CONFIG")
            .unwrap_or_else(|_| "./config.toml".to_owned())
            .parse::<PathBuf>()
            .expect("file path to be valid");
        let config = std::fs::read_to_string(config)
            .map(|config_file| {
                toml::from_str::<NodeConfig>(&config_file).expect("invalid config file")
            })
            .unwrap_or_else(|err| {
                tracing::warn!(
                    custom.error = ?err,
                    "config file not provided, defaulting to testnet config"
                );
                NodeConfig::testnet()
            });

        // open the Irys database
        let db_path = config.irys_consensus_data_dir();

        Arc::new(DatabaseEnv::open(
            &db_path,
            DatabaseEnvKind::RO,
            irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )?)
    };

    let tx = db.tx()?;
    let mut read_cursor = tx.cursor_read::<irys_database::tables::IrysBlockHeaders>()?;
    let mut walker = read_cursor.walk(None)?;
    while let Some((hash, header)) = walker.next().transpose()? {
        if header.height % 1000 == 0 {
            tracing::debug!("Processed block {}", &header.height)
        };

        let ledger =
            match header.system_ledgers.iter().find(|tx_ledger| {
                tx_ledger.ledger_id == irys_types::SystemLedger::Commitment as u32
            }) {
                Some(ledger) => ledger,
                None => continue,
            };

        for id in ledger.tx_ids.0.iter() {
            let tx = commitment_tx_by_txid(&tx, id)?.unwrap();
            tracing::debug!("got commitment {:#?} in block {}", &tx, &header.height);
        }

        if !ledger.tx_ids.is_empty() {
            tracing::debug!(
                "block {} {} has commitment txs {:?}",
                &header.height,
                &hash,
                &ledger.tx_ids
            );
        }
    }

    Ok(())
}
