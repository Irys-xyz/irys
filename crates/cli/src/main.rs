use clap::{command, Parser, Subcommand};
use irys_config::IrysNodeConfig;
use irys_database::reth_db::{
    cursor::*, init_db, transaction::*, Database as _, PlainAccountState, StageCheckpoints,
};
use irys_types::Config;
use reth_node_core::version::default_client_version;
use reth_tracing::tracing_subscriber::util::SubscriberInitExt as _;
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::{path::PathBuf, sync::Arc};
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt as _, EnvFilter, Layer as _, Registry};

#[derive(Debug, Parser, Clone)]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand, Clone)]
pub enum Commands {
    #[command(name = "backup-accounts")]
    BackupAccounts {
        #[clap(last = true)]
        args: Vec<String>,
    },
}

fn main() -> eyre::Result<()> {
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
        Commands::BackupAccounts { .. } => backup_accounts()?,
    }
    Ok(())
}

fn backup_accounts() -> eyre::Result<()> {
    // load the config
    let config_file = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("invalid file path");

    let config = std::fs::read_to_string(config_file).map_or_else(
        |_err| {
            tracing::warn!("config file not provided, defaulting to testnet config");
            Config::testnet()
        },
        |config_file| toml::from_str::<Config>(&config_file).expect("invalid config file"),
    );
    let irys_node_config = IrysNodeConfig::new(&config);

    // open the database, read the current account state

    let db_path = irys_node_config.reth_data_dir().join("db");

    let reth_db = Arc::new(
        init_db(
            db_path,
            irys_database::reth_db::mdbx::DatabaseArguments::new(default_client_version())
                .with_log_level(None)
                .with_exclusive(Some(false)),
        )?
        .with_metrics(),
    );

    let read_tx = reth_db.tx()?;
    // read the latest block
    let latest_reth_block = read_tx
        .get::<StageCheckpoints>("Finish".to_owned())?
        .map(|ch| ch.block_number)
        .expect("unable to get latest reth block");

    let row_count = read_tx.entries::<PlainAccountState>()?;

    info!(
        "Saving {} accounts @ block {}",
        &row_count, &latest_reth_block
    );
    let mut read_cursor = read_tx.cursor_read::<PlainAccountState>()?;

    let mut walker = read_cursor.walk(None)?;

    let file = File::create(format!("accounts-{}.json", &latest_reth_block))?;
    let mut writer = BufWriter::new(file);

    writer.write_all(b"[\n")?;

    let mut accounts_saved = 0;
    let log_batch = 100;

    while let Some(account) = walker.next().transpose()? {
        serde_json::to_writer(&mut writer, &account)?;

        accounts_saved += 1;

        // omit the trailing comma
        writer.write_all(if accounts_saved == row_count {
            b"\n"
        } else {
            b",\n"
        })?;
        if accounts_saved % log_batch == 0 {
            info!("Saved {}/{} accounts", &accounts_saved, &row_count);
        }
    }

    writer.write_all(b"]")?;
    writer.flush()?;

    read_tx.commit()?;

    info!("Accounts saved!");

    Ok(())
}
