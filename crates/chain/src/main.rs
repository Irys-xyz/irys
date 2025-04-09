use std::path::PathBuf;

use irys_chain::IrysNode;
use irys_types::Config;
use reth_primitives::GenesisAccount;
use reth_tracing::tracing_subscriber::util::SubscriberInitExt;
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    // init logging
    init_tracing().expect("initializing tracing should work");
    color_eyre::install().expect("color eyre could not be installed");

    // load the config
    let config = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");
    let config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<Config>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|_err| {
            tracing::warn!("config file not provided, defaulting to testnet config");
            Config::testnet()
        });

    // check env var to see if we are starting up in Genesis mode
    let is_genesis = std::env::var("GENESIS").map(|_| true).unwrap_or(false);

    // start the node
    tracing::info!("starting the node");
    let mut node = IrysNode::new(config, is_genesis);
    if is_genesis {
        let file = std::fs::File::open("genesis-accounts.json")?;
        let accounts: Vec<(irys_types::Address, reth_primitives::Account)> =
            serde_json::from_reader(file)?;
        node.irys_node_config
            .extend_genesis_accounts(accounts.iter().map(|(addr, acc)| {
                let mapped_acc = GenesisAccount {
                    balance: acc.balance,
                    nonce: None,
                    ..Default::default()
                };
                (*addr, mapped_acc)
            }));
        node.irys_node_config.extend_genesis_accounts([
            (
                irys_types::Address::from_slice(
                    hex::decode("64f1a2829e0e698c18e7792d6e74f67d89aa0a32")
                        .unwrap()
                        .as_slice(),
                ),
                GenesisAccount {
                    balance: alloy_core::primitives::U256::from(690000000000000000_u128),
                    ..Default::default()
                },
            ),
            (
                irys_types::Address::from_slice(
                    hex::decode("A93225CBf141438629f1bd906A31a1c5401CE924")
                        .unwrap()
                        .as_slice(),
                ),
                GenesisAccount {
                    balance: alloy_core::primitives::U256::from(
                        1_000_000_000_000_000_000_000_000_000_000_u128,
                    ),
                    ..Default::default()
                },
            ),
        ]);
    }
    let handle = node.start().await?;
    handle.start_mining()?;

    // wait for the node to be shut down
    tokio::task::spawn_blocking(|| {
        handle.reth_thread_handle.unwrap().join().unwrap();
    })
    .await?;

    Ok(())
}

fn init_tracing() -> eyre::Result<()> {
    let subscriber = Registry::default();
    let filter =
        EnvFilter::new("info").add_directive(EnvFilter::from_default_env().to_string().parse()?);

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stdout);

    // use json logging for release builds
    let subscriber = subscriber.with(filter).with(ErrorLayer::default());
    let subscriber = if cfg!(debug_assertions) {
        subscriber.with(output_layer.boxed())
    } else {
        subscriber.with(output_layer.json().with_current_span(true).boxed())
    };

    subscriber.init();

    Ok(())
}
