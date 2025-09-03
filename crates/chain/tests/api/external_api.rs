//! endpoint tests
use crate::{
    api::{
        block_index_endpoint_request, chunk_endpoint_request, info_endpoint_request,
        network_config_endpoint_request, peer_list_endpoint_request, version_endpoint_request,
    },
    utils::IrysNodeTest,
};
use actix_web::{http::header::ContentType, HttpMessage as _};
use irys_testing_utils::initialize_tracing;
use irys_types::{BlockIndexItem, NodeInfo};
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

    // advance enough blocks to cause 2 blocks to migrate from mempool to index
    ctx.mine_blocks(block_migration_depth as usize + 2).await?;

    // wait for 2 blocks in the index
    if let Err(e) = ctx.wait_until_block_index_height(2, 10).await {
        panic!("Error waiting for block height on chain. Error: {:?}", e);
    }

    let mut response = info_endpoint_request(&address).await;

    // deserialize the response into NodeInfo struct
    let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");

    // The block index is 0-based, so the total number of entries is height + 1.
    let total_entries = json_response.block_index_height + 1;

    // check the api endpoint again, and it should now show 2 blocks are in the index
    assert_eq!(json_response.block_index_height, 2);

    // For this endpoint:
    // - height is the start index (0-based), inclusive
    // - limit == 0 means "use default limit", which is large enough to include all
    //   remaining items from height onward, so expected = total_entries - height
    // - otherwise, expected = min(limit, total_entries - height)
    fn expected_count(total_entries: u64, height: u64, limit: u64) -> u64 {
        let remaining = total_entries.saturating_sub(height);
        if limit == 0 {
            remaining
        } else {
            remaining.min(limit)
        }
    }

    // tests should check total number of json objects returned are equal to the expected number.
    for limit in 0..2 {
        for height in 0..2 {
            let mut response = block_index_endpoint_request(&address, height, limit).await;
            assert_eq!(response.status(), 200);
            assert_eq!(response.content_type(), ContentType::json().to_string());
            let items: Vec<BlockIndexItem> = response.json().await.expect("valid BlockIndexItem");
            let expected = expected_count(total_entries, height, limit);
            assert_eq!(
                items.len() as u64,
                expected,
                "unexpected length for height={}, limit={}, total_entries={}",
                height,
                limit,
                total_entries
            );
        }
    }

    ctx.node_ctx.stop().await;
    Ok(())
}
