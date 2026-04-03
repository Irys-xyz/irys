mod cli_args;
mod commands;
mod db_utils;
mod snapshot_output;

#[cfg(test)]
mod tests;

use clap::Parser as _;
use tracing::level_filters::LevelFilter;
use tracing_error::ErrorLayer;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt as _};

use crate::cli_args::IrysCli;

fn init_tracing() -> eyre::Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()?;

    Registry::default()
        .with(filter)
        .with(ErrorLayer::default())
        .with(irys_utils::make_fmt_layer())
        .init();

    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    init_tracing()?;
    color_eyre::install().expect("color eyre could not be installed");
    let args = IrysCli::parse();
    commands::run(args).await
}
