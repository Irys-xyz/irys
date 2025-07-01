//! endpoint tests
use crate::{
    api::{
        block_index_endpoint_request, chunk_endpoint_request, info_endpoint_request,
        network_config_endpoint_request, peer_list_endpoint_request, version_endpoint_request,
    },
    utils::IrysNodeTest,
};
use actix_web::{http::header::ContentType, HttpMessage as _};
use irys_api_server::routes::index::NodeInfo;
use irys_testing_utils::initialize_tracing;
use irys_types::BlockIndexItem;
use tracing::info;

#[actix::test]
async fn heavy_external_api() -> eyre::Result<()> {
    initialize_tracing();

    let ctx = IrysNodeTest::default_async().start().await;

    // retrieve block_migration_depth for use later
    let mut consensus = ctx.cfg.consensus.clone();
    let block_migration_depth = consensus.get_mut().block_migration_depth;

    let address = format!(
        "http://127.0.0.1:{}",
        ctx.node_ctx.config.node_config.http.bind_port
    );

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

    // advance enough blocks to cause 1 block to migrate from mempool to index
    ctx.mine_blocks(block_migration_depth as usize + 1).await?;

    // wait for 1 block in the index
    if let Err(e) = ctx.wait_until_height_on_chain(1, 10).await {
        panic!("Error waiting for block height on chain. Error: {:?}", e);
    }

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

    ctx.node_ctx.stop().await;
    Ok(())
}
