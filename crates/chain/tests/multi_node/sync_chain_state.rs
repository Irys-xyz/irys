use crate::utils::mine_blocks;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::peer_utilities::{block_index_endpoint_request, info_endpoint_request};
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_database::BlockIndexItem;
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::Config;
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

/// spin up a genesis node and two peers. Check that we can sync blocks from the genesis node
/// check that the blocks ar evalid, check that peer1, peer2, and gensis are indeed synced
#[test_log::test(actix_web::test)]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let required_blocks_height: usize = 5;
    // +2 is so genesis is two blocks ahead of the peer nodes, as currently we check the peers index which lags behind
    let required_genesis_node_height = required_blocks_height + 2;
    let trusted_peers = vec!["127.0.0.1:8080".parse().expect("valid SocketAddr expected")];
    let testnet_config_genesis = Config {
        port: 8080,
        trusted_peers: trusted_peers.clone(),
        ..Config::testnet()
    };
    let ctx_genesis_node = setup_with_config(
        testnet_config_genesis.clone(),
        "heavy_sync_chain_state_genesis",
        true,
    )
    .await
    .expect("found invalid genesis ctx");
    // mine x blocks
    mine_blocks(&ctx_genesis_node.node, required_genesis_node_height)
        .await
        .expect("expected many mined blocks");

    //start two additional peers, instructing them to use the genesis peer as their trusted peer

    //start peer1
    let testnet_config_peer1 = Config {
        port: 0, //random port
        trusted_peers: trusted_peers.clone(),
        ..Config::testnet()
    };
    let ctx_peer1_node = setup_with_config(
        testnet_config_peer1.clone(),
        "heavy_sync_chain_state_peer1",
        false,
    )
    .await
    .expect("found invalid genesis ctx for peer1");

    //start peer2
    let testnet_config_peer2 = Config {
        port: 0, //random port
        trusted_peers,
        ..Config::testnet()
    };
    let ctx_peer2_node = setup_with_config(
        testnet_config_peer2.clone(),
        "heavy_sync_chain_state_peer2",
        false,
    )
    .await
    .expect("found invalid genesis ctx for peer2");

    // check the height returned by the peers, and when it is high enough do the api call for the block_index and then shutdown the peer
    // this should expand with the block height
    let max_attempts: u64 = required_blocks_height
        .try_into()
        .expect("expected required_blocks_height to be valid u64");
    let max_attempts = max_attempts * 3;

    let result_peer1 = poll_until_fetch_at_block_index_height(
        &ctx_peer1_node,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    //shut down peer, we have what we need
    ctx_peer1_node.node.stop().await;

    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    //shut down peer, we have what we need
    ctx_peer2_node.node.stop().await;

    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.port),
        0,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;

    //shutdown genesis node, as the peers are no longer going make http calls to it
    ctx_genesis_node.node.stop().await;

    // compare blocks in indexes from each of the three nodes
    // they should be identical if the sync was a success
    let block_index_genesis = result_genesis
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    let block_index_peer1 = result_peer1
        .expect("expected a client response from peer1")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    let block_index_peer2 = result_peer2
        .expect("expected a client response from peer2")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");

    assert_eq!(
        block_index_genesis, block_index_peer1,
        "expecting json from genesis node {:?} to match json from peer1 {:?}",
        block_index_genesis, block_index_peer1
    );
    assert_eq!(
        block_index_peer1, block_index_peer2,
        "expecting json from peer1 node {:?} to match json from peer2 {:?}",
        block_index_peer1, block_index_peer2
    );

    Ok(())
}

struct TestCtx {
    node: IrysNodeCtx,
    #[expect(
        dead_code,
        reason = "to prevent drop() being called and cleaning up resources"
    )]
    temp_dir: TempDir,
}

async fn setup_with_config(
    mut testnet_config: Config,
    node_name: &str,
    genesis: bool,
) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some(node_name), false);
    testnet_config.base_directory = temp_dir.path().to_path_buf();
    let mut irys_node = IrysNode::new(testnet_config.clone(), genesis).await;
    let node = irys_node.start().await?;
    Ok(TestCtx { node, temp_dir })
}

fn local_test_url(port: &u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

async fn poll_until_fetch_at_block_index_height(
    node_ctx: &TestCtx,
    required_blocks_height: u64,
    max_attempts: u64,
) -> Option<awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>>> {
    let mut attempts = 0;
    let mut result_peer = None;
    let url = local_test_url(&node_ctx.node.config.port);
    loop {
        let mut response = info_endpoint_request(&url).await;

        if max_attempts < attempts {
            error!("peer never fully synced");
            break;
        } else {
            attempts += 1;
        }

        let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");
        if required_blocks_height > json_response.block_index_height {
            debug!(
                "attempt {} checking {}. required_blocks_height > json_response.block_index_height {} > {}",
                &attempts, &url, required_blocks_height, json_response.block_index_height
            );
            //wait 5 seconds and try again
            sleep(Duration::from_millis(5000)).await;
        } else {
            result_peer = Some(
                block_index_endpoint_request(
                    &local_test_url(&node_ctx.node.config.port),
                    0,
                    required_blocks_height,
                )
                .await,
            );
        }
    }
    result_peer
}
