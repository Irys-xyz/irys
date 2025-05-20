use irys_chain::IrysNode;
use irys_types::{NodeConfig, NodeMode};
use std::path::PathBuf;
use tracing::{debug, info, warn};
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    layer::SubscriberExt, util::SubscriberInitExt as _, EnvFilter, Layer, Registry,
};

#[actix_web::main]
async fn main() -> eyre::Result<()> {
    // init logging
    init_tracing().expect("initializing tracing should work");
    color_eyre::install().expect("color eyre could not be installed");

    // load the config
    let config_path = std::env::var("CONFIG")
        .map(|s| s.parse::<PathBuf>().expect("file path to be valid"))
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .expect("Unable to determine working dir, aborting")
                .join("config.toml")
        });

    let mut config = std::fs::read_to_string(&config_path)
        .map(|config_file| {
            debug!("Loading config from {:?}", &config_path);
            toml::from_str::<NodeConfig>(&config_file).expect("invalid config file")
        })
        .unwrap_or_else(|err| {
            warn!(
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
    info!("starting the node, mode: {:?}", &config.mode);
    let handle = IrysNode::new(config).await?.start().await?;
    handle.start_mining().await?;
    let reth_thread_handle = handle.reth_thread_handle.clone();
    // wait for the node to be shut down
    tokio::task::spawn_blocking(|| {
        reth_thread_handle.unwrap().join().unwrap();
    })
    .await?;

    handle.stop().await;

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
