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

    // // !!!TESTNET ONLY!!!
    // {
    //     debug!("Loading accounts file");
    //     let file = std::fs::File::open("genesis-accounts.json")?;
    //     let reader = std::io::BufReader::with_capacity(10 * 1024 * 1024, file);
    //     let accounts: Vec<(irys_types::Address, reth_primitives::Account)> =
    //         serde_json::from_reader(reader)?;
    //     debug!("extending accounts");
    //     config
    //         .consensus
    //         .extend_genesis_accounts(accounts.iter().map(|(addr, acc)| {
    //             let mapped_acc = reth_primitives::GenesisAccount {
    //                 balance: acc.balance,
    //                 nonce: None,
    //                 ..Default::default()
    //             };
    //             (*addr, mapped_acc)
    //         }));
    //     debug!("Finished loading accounts");
    //     config.consensus.extend_genesis_accounts([
    //         (
    //             irys_types::Address::from_slice(
    //                 hex::decode("6f8450cfdb7c9aeddab081a5cf43755201f69582")
    //                     .unwrap()
    //                     .as_slice(),
    //             ),
    //             reth_primitives::GenesisAccount {
    //                 balance: alloy_core::primitives::U256::from(690000000000000000_u128),
    //                 ..Default::default()
    //             },
    //         ),
    //         (
    //             irys_types::Address::from_slice(
    //                 hex::decode("A93225CBf141438629f1bd906A31a1c5401CE924")
    //                     .unwrap()
    //                     .as_slice(),
    //             ),
    //             reth_primitives::GenesisAccount {
    //                 balance: alloy_core::primitives::U256::from(
    //                     1_000_000_000_000_000_000_000_000_000_000_000_000_u128,
    //                 ),
    //                 ..Default::default()
    //             },
    //         ),
    //     ]);
    // }

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
    // let subscriber = if cfg!(debug_assertions) {
    //     subscriber.with(output_layer.boxed())
    // } else {
    //     subscriber.with(output_layer.json().with_current_span(true).boxed())
    // };
    let subscriber = subscriber.with(output_layer.boxed());

    subscriber.init();

    Ok(())
}
