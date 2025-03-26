//! endpoint tests
use std::sync::Arc;

use crate::utils::mine_block;
use actix_web::{http::header::ContentType, HttpMessage};
use irys_actors::BlockFinalizedMessage;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::{start_irys_node, IrysNodeCtx};
use irys_config::IrysNodeConfig;
use irys_database::BlockIndexItem;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::{Address, Config, IrysTransactionHeader, Signature, H256};
use tokio::time::{sleep, Duration};
use tracing::info;

async fn client_request(
    url: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    let client = awc::Client::default();

    client.get(url).send().await.expect("client request")
}

async fn info_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/info")).await
}

async fn block_index_endpoint_request(
    address: &str,
    height: u64,
    limit: u64,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!(
        "{}{}?height={}&limit={}",
        &address, "/v1/block_index", &height, &limit
    ))
    .await
}

async fn chunk_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/chunk/ledger/0/0")).await
}

async fn network_config_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/network/config")).await
}

async fn peer_list_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/peer_list")).await
}

async fn version_endpoint_request(
    address: &str,
) -> awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>> {
    client_request(&format!("{}{}", &address, "/v1/version")).await
}

#[actix::test]
async fn serial_external_api() -> eyre::Result<()> {
    let ctx = setup().await?; // start api service

    let address = "http://127.0.0.1:8080";

    // FIXME: Test to be updated with future endpoint work
    let mut _response = chunk_endpoint_request(&address).await;
    //assert_eq!(_response.status(), 200);
    //assert_eq!(_response.content_type(), ContentType::json());

    let mut _response = network_config_endpoint_request(&address).await;
    assert_eq!(_response.status(), 200);
    assert_eq!(_response.content_type(), ContentType::json().to_string());

    let mut _response = peer_list_endpoint_request(&address).await;
    assert_eq!(_response.status(), 200);
    assert_eq!(_response.content_type(), ContentType::json().to_string());

    // FIXME: Test to be updated with future endpoint work
    let mut _response = version_endpoint_request(&address).await;
    //assert_eq!(response.status(), 200);
    //assert_eq!(response.content_type(), ContentType::json());

    let mut response = info_endpoint_request(&address).await;

    assert_eq!(response.status(), 200);
    info!("HTTP server started");

    // confirm we are receiving the correct content type
    assert_eq!(response.content_type(), ContentType::json().to_string());

    // deserialize the response into NodeInfo struct
    let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");

    assert_eq!(json_response.block_index_height, 0);

    // advance one block
    let (_header, _payload) = mine_block(&ctx.node).await?.unwrap();
    // advance one block, finalizing the previous block
    let (header, _payload) = mine_block(&ctx.node).await?.unwrap();

    let mock_header = IrysTransactionHeader {
        id: H256::from([255u8; 32]),
        anchor: H256::from([1u8; 32]),
        signer: Address::default(),
        data_root: H256::from([3u8; 32]),
        data_size: 1024,
        term_fee: 100,
        perm_fee: Some(200),
        ledger_id: 1,
        bundle_format: None,
        chain_id: ctx.config.chain_id,
        version: 0,
        ingress_proofs: None,
        signature: Signature::test_signature().into(),
    };

    let block_finalized_message = BlockFinalizedMessage {
        block_header: header,
        all_txs: Arc::new(vec![mock_header]),
    };

    //FIXME: magic number could be a constant e.g. 3 blocks worth of time?
    sleep(Duration::from_millis(10000)).await;

    let _ = ctx
        .node
        .actor_addresses
        .block_index
        .send(block_finalized_message);

    let mut response = info_endpoint_request(&address).await;

    // deserialize the response into NodeInfo struct
    let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");

    // check the api endpoint again, and it should now show 1 block in the index
    assert_eq!(json_response.block_index_height, 1);

    // tests should check total number of json objects returned are equal to the number requested.
    // Ideally should also check that the expected fields of those objects are present.
    for limit in 0..2 {
        for height in 0..2 {
            let mut response = block_index_endpoint_request(&address, height, limit).await;
            assert_eq!(response.status(), 200);
            assert_eq!(response.content_type(), ContentType::json().to_string());
            let json_response: Vec<BlockIndexItem> =
                response.json().await.expect("valid BlockIndexItem");
            assert_eq!(json_response.len() as u64, limit);
        }
    }

    ctx.node.stop().await;
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
