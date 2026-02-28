use clap::{CommandFactory as _, FromArgMatches as _};
use irys_chain::{
    cli::{merge::apply_cli_overrides, Commands, IrysCli, NodeCommand},
    utils::{load_config, load_config_from_path},
    IrysNode,
};
use irys_testing_utils::setup_panic_hook;
use irys_types::{NodeConfig, ShutdownReason};
use irys_utils::shutdown::spawn_shutdown_watchdog;
use std::path::PathBuf;
use tracing::{error, info, level_filters::LevelFilter};
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Registry,
};

#[cfg(feature = "telemetry")]
use irys_utils::init_telemetry;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[tokio::main]
#[tracing::instrument(level = "trace", skip_all)]
async fn main() -> eyre::Result<()> {
    // Load .env file if present (silently ignore if not found)
    let _ = dotenvy::dotenv();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "full") };
    }

    #[cfg(feature = "telemetry")]
    {
        let telemetry_enabled = std::env::var("ENABLE_TELEMETRY")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false)
            || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                .map(|v| !v.is_empty())
                .unwrap_or(false);

        if telemetry_enabled {
            init_telemetry()?;
            info!("Telemetry enabled, OpenTelemetry initialized");
        } else {
            init_tracing().expect("initializing tracing should work");
            info!("Telemetry not enabled, using standard tracing");
        }
    }
    #[cfg(not(feature = "telemetry"))]
    {
        init_tracing().expect("initializing tracing should work");
    }

    setup_panic_hook().expect("custom panic hook installation to succeed");
    reth_cli_util::sigsegv_handler::install();

    // Parse CLI — get both typed struct and raw ArgMatches for value_source() checks
    let matches = IrysCli::command().get_matches();
    let cli = IrysCli::from_arg_matches(&matches)?;

    let config = match cli.command {
        Some(Commands::Node(cmd)) => {
            // Extract the "node" subcommand matches for value_source() checks
            let node_matches = matches
                .subcommand_matches("node")
                .expect("Node subcommand must have matches");
            run_node_config(*cmd, node_matches)?
        }
        None => load_config()?,
    };

    start_node(config).await
}

/// Resolve config for the `irys node` subcommand: load TOML then apply CLI overrides.
fn run_node_config(cmd: NodeCommand, matches: &clap::ArgMatches) -> eyre::Result<NodeConfig> {
    let config_path = cmd
        .config
        .clone()
        .or_else(|| std::env::var("CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    if cmd.generate_config {
        load_config_from_path(&config_path, true)?;
        unreachable!("load_config_from_path with generate=true always returns Err");
    }

    let config = load_config_from_path(&config_path, false)?;
    apply_cli_overrides(config, &cmd, matches)
}

/// Start the node with a fully resolved config.
async fn start_node(config: NodeConfig) -> eyre::Result<()> {
    info!("starting the node, mode: {:?}", &config.node_mode);
    let (config, http_listener, gossip_listener) = IrysNode::bind_listeners(config)?;
    let handle = IrysNode::new_with_listeners(config, http_listener, gossip_listener)?
        .start()
        .await?;
    handle.start_mining()?;

    // Await reth thread completion asynchronously
    // Brief non-contended lock to extract the oneshot receiver.
    // std::sync::Mutex is intentional: held only for .take(), no contention.
    let reth_done_rx = handle.reth_done_rx.lock().unwrap().take();
    let shutdown_reason = match reth_done_rx {
        Some(rx) => match rx.await {
            Ok(reason) => reason,
            Err(_) => {
                error!("Reth completion sender dropped without sending (thread may have panicked)");
                ShutdownReason::FatalError(
                    "Reth completion sender dropped without sending".to_string(),
                )
            }
        },
        None => {
            error!("Reth completion receiver was None");
            ShutdownReason::FatalError("Reth completion receiver was None".to_string())
        }
    };

    // Spawn watchdog thread to force exit if graceful shutdown hangs
    spawn_shutdown_watchdog(shutdown_reason.clone());

    handle.stop(shutdown_reason).await;

    Ok(())
}

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
