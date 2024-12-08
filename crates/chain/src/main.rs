use clap::{command, Parser};
use irys_chain::chain::start_irys_node;
use reth_tracing::tracing_subscriber::fmt::SubscriberBuilder;
use reth_tracing::tracing_subscriber::util::SubscriberInitExt;
use tracing::level_filters::LevelFilter;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long, default_value = "./database")]
    database: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // TODO: fix this, we used to await the reth node exit future but can't anymore
    // so we need another near-infinite blocking future
    let _ = SubscriberBuilder::default()
        .with_max_level(LevelFilter::INFO)
        .finish()
        .try_init();

    let handle = start_irys_node(Default::default()).await?;
    handle.actor_addresses.start_mining()?;
    std::thread::park();

    Ok(())
}
