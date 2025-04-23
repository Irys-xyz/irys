use std::path::PathBuf;

use irys_chain::IrysNode;
use irys_types::{NodeConfig, NodeMode};
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
    let mut config = std::fs::read_to_string(config)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
        .unwrap_or_else(|err| {
            tracing::warn!(
                ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });
    let is_genesis = std::env::var("GENESIS").map(|_| true).unwrap_or(false);
    if is_genesis {
        config.mode = NodeMode::Genesis;
    } else {
        config.mode = NodeMode::PeerSync;
    }

    // start the node
    tracing::info!("starting the node");
    let handle = IrysNode::new(config).await?.start().await?;
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
