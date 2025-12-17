use irys_chain::{utils::load_config, IrysNode};
use irys_testing_utils::setup_panic_hook;
use irys_types::ShutdownReason;
use irys_utils::shutdown::spawn_shutdown_watchdog;
use tracing::{error, info, level_filters::LevelFilter};
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Layer as _, Registry,
};

#[cfg(feature = "telemetry")]
use irys_utils::telemetry;

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

    // init logging
    #[cfg(feature = "telemetry")]
    {
        // Check if Axiom credentials are set
        if std::env::var("AXIOM_API_TOKEN").is_ok() && std::env::var("AXIOM_DATASET").is_ok() {
            info!("Axiom credentials detected, initializing OpenTelemetry");
            telemetry::init_telemetry()?;
        } else {
            info!("Axiom credentials not set, using standard tracing");
            init_tracing().expect("initializing tracing should work");
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
    let shutdown_reason = tokio::task::spawn_blocking(move || {
        match reth_thread_handle {
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
        }
    })
    .await?;

    // Spawn watchdog thread to force exit if graceful shutdown hangs
    spawn_shutdown_watchdog(shutdown_reason.clone());

    handle.stop(shutdown_reason).await;

    Ok(())
}

fn init_tracing() -> eyre::Result<()> {
    let subscriber = Registry::default();
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()?;

    let output_layer = tracing_subscriber::fmt::layer()
        .with_line_number(true)
        .with_ansi(true)
        .with_file(true)
        .with_writer(std::io::stdout);

    // use json logging for release builds
    let subscriber = subscriber.with(filter).with(ErrorLayer::default());
    // TODO: re-enable with config options

    // let subscriber = if cfg!(debug_assertions) {
    //     subscriber.with(output_layer.boxed())
    // } else {
    //     subscriber.with(output_layer.json().with_current_span(true).boxed())
    // };
    let subscriber = subscriber.with(output_layer.boxed());

    subscriber.init();

    Ok(())
}
