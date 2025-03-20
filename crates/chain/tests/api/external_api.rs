//! chunk migration tests
use std::time::Duration;

use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{
    setup_tracing_and_temp_dir, tempfile::TempDir, temporary_directory,
};
use irys_types::{Config, StorageConfig};
use tokio::time::sleep;
use tracing::info;

#[actix::test]
async fn external_api() -> eyre::Result<()> {
    let _ctx = setup().await?;

    let address = "http://127.0.0.1:8080";
    let client = awc::Client::default();

    let response = client
        .get(format!("{}/v1/info", address))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    info!("HTTP server started");

    Ok(())
}

struct TestCtx {
    config: Config,
    node: IrysNodeCtx,
    #[expect(
        dead_code,
        reason = "to prevent drop() being called and cleaning up resources"
    )]
    temp_dir: TempDir,
}

async fn setup() -> eyre::Result<TestCtx> {
    let testnet_config = Config {
        // add any overrides here
        ..Config::testnet()
    };
    setup_with_config(testnet_config).await
}

async fn setup_with_config(testnet_config: Config) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some("external_api"), false);
    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();
    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    Ok(TestCtx {
        config: testnet_config,
        node,
        temp_dir,
    })
}
