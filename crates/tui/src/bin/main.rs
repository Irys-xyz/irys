use clap::Parser;
use eyre::Result;
use irys_tui::{app::App, utils::terminal};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(Parser, Debug)]
#[command(name = "irys-tui")]
#[command(about = "Irys cluster monitoring TUI")]
struct Cli {
    #[arg(value_name = "NODE_URLS", help = "Node URLs to connect to")]
    node_urls: Vec<String>,

    #[arg(
        short,
        long,
        help = "Configuration file path (contains both TUI settings and nodes list)"
    )]
    config: Option<String>,

    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cli.log_level.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Check if we have any node configuration
    if cli.node_urls.is_empty() && cli.config.is_none() {
        eprintln!("Error: No nodes specified.");
        eprintln!("\nYou must provide nodes via one of the following methods:");
        eprintln!("  1. Command line arguments: irys-tui http://node1:port http://node2:port");
        eprintln!("  2. Config file: irys-tui --config tui.toml");
        eprintln!("\nExample:");
        eprintln!("  irys-tui http://localhost:19080 http://localhost:19081");
        eprintln!("  irys-tui --config tui.toml");
        eprintln!("\nExample config file (tui.toml):");
        eprintln!("  refresh_interval_secs = 30");
        eprintln!("  connection_timeout_secs = 10");
        eprintln!("  ");
        eprintln!("  [[nodes]]");
        eprintln!("  url = \"http://localhost:19080\"");
        eprintln!("  alias = \"Primary Node\"");
        eprintln!("  ");
        eprintln!("  [[nodes]]");
        eprintln!("  url = \"http://localhost:19081\"");
        eprintln!("  alias = \"Secondary Node\"");
        std::process::exit(1);
    }

    let mut terminal = terminal::init()?;
    let mut app = App::new(cli.node_urls, cli.config)?;
    let app_result = app.run(&mut terminal).await;

    terminal::restore()?;

    app_result
}
