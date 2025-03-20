//! endpoint tests
use actix_web::HttpMessage;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::Config;
use tracing::info;

#[actix::test]
async fn external_api() -> eyre::Result<()> {
    let _ctx = setup().await?;

    let address = "http://127.0.0.1:8080";
    let client = awc::Client::default();

    let mut response = client
        .get(format!("{}/v1/info", address))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    info!("HTTP server started");

    // confirm we are recieving the correct content type
    assert_eq!(response.content_type(), "application/json");

    // deserialize the response into NodeInfo struct
    let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");

    assert_eq!(json_response.block_index_height, 0);

    Ok(())
}

struct TestCtx {
    _config: Config,
    _node: IrysNodeCtx,
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
        _config: testnet_config,
        _node: node,
        temp_dir,
    })
}
