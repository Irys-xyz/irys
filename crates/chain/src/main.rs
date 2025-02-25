use std::path::PathBuf;

use irys_chain::chain::start;
use irys_types::Config;
use reth_tracing::tracing_subscriber::util::SubscriberInitExt;
use tracing_error::ErrorLayer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // init logging
    init_tracing().expect("initializing tracing should work");
    color_eyre::install().expect("color eyre could not be installed");

    // load the config
    let config_file = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("invalid file path");
    let config_file = std::fs::read_to_string(config_file).expect("cannot read config file");
    let config = toml::from_str::<Config>(&config_file).expect("invalid config file");

    // start the node
    let handle = start(config).await?;
    handle.actor_addresses.start_mining()?;
    std::thread::park();

    Ok(())
}

fn init_tracing() -> eyre::Result<()> {
    let subscriber = Registry::default();
    let filter = EnvFilter::new("actix=info")
        .add_directive("actix_web=info".parse()?)
        .add_directive(EnvFilter::from_default_env().to_string().parse()?);

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
