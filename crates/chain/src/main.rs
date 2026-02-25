use irys_chain::{utils::load_config, IrysNode};
use irys_testing_utils::setup_panic_hook;
use irys_types::ShutdownReason;
use irys_utils::shutdown::spawn_shutdown_watchdog;
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
    // load the config
    let config = load_config()?;

    // start the node
    info!("starting the node, mode: {:?}", &config.node_mode);
    let handle = IrysNode::new(config)?.start().await?;
    handle.start_mining()?;
    let reth_thread_handle = handle.reth_thread_handle.clone();
    // wait for the node to be shut down
    let shutdown_reason = tokio::task::spawn_blocking(move || match reth_thread_handle {
        Some(handle) => match handle.join() {
            Ok(reason) => reason,
            Err(e) => {
                error!("Reth thread panicked: {:?}", e);
                ShutdownReason::FatalError("Reth thread panicked".to_string())
            }
        },
        None => {
            error!("Reth thread handle was None");
            ShutdownReason::FatalError("Reth thread handle was None".to_string())
        }
    })
    .await?;

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
