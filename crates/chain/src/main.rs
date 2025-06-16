use irys_chain::{utils::load_config, IrysNode};
use tracing::info;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter, Layer as _, Registry,
};

#[actix_web::main]
async fn main() -> eyre::Result<()> {
    // init logging
    init_tracing().expect("initializing tracing should work");
    color_eyre::install().expect("color eyre could not be installed");

    // load the config
    let config = load_config()?;

    // start the node
    info!("starting the node, mode: {:?}", &config.mode);
    let handle = IrysNode::new(config)?.start().await?;
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
