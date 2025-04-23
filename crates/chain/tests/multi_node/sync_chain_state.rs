use crate::utils::{mine_blocks, IrysNodeTest};
use irys_api_server::routes::index::NodeInfo;
use irys_chain::peer_utilities::{
    block_index_endpoint_request, info_endpoint_request, peer_list_endpoint_request,
};
use irys_chain::IrysNodeCtx;
use irys_database::BlockIndexItem;
use irys_types::{NodeConfig, NodeMode, PeerAddress};
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

/// spin up a genesis node and two peers. Check that we can sync blocks from the genesis node
/// check that the blocks are valid, check that peer1, peer2, and gensis are indeed synced
#[test_log::test(actix_web::test)]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let required_blocks_height: usize = 5;
    // +2 is so genesis is two blocks ahead of the peer nodes, as currently we check the peers index which lags behind
    let required_genesis_node_height = required_blocks_height + 2;
    let trusted_peers = vec![PeerAddress {
        api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
        gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
    }];
    let genesis_trusted_peers = vec![
        PeerAddress {
            api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
        },
        PeerAddress {
            api: "127.0.0.2:1234".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.2:1235".parse().expect("valid SocketAddr expected"),
        },
        PeerAddress {
            api: "127.0.0.3:1234".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.3:1235".parse().expect("valid SocketAddr expected"),
        },
    ];
    let mut testnet_config_genesis = NodeConfig::testnet();
    testnet_config_genesis.http.port = 8080;
    testnet_config_genesis.gossip.port = 8081;
    testnet_config_genesis.trusted_peers = genesis_trusted_peers.clone();
    testnet_config_genesis.mode = NodeMode::Genesis;
    let ctx_genesis_node = IrysNodeTest::new_genesis(testnet_config_genesis.clone())
        .await
        .start()
        .await;

    // mine x blocks
    mine_blocks(&ctx_genesis_node.node_ctx, required_genesis_node_height)
        .await
        .expect("expected many mined blocks");

    // wait and retry hitting the peer_list endpoint of genesis node
    let peer_list_items =
        poll_peer_list(genesis_trusted_peers.clone(), &ctx_genesis_node.node_ctx).await;
    // assert that genesis node is advertising the trusted peers it was given via config
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    //start two additional peers, instructing them to use the genesis peer as their trusted peer

    //start peer1
    let mut testnet_config_peer1 = NodeConfig::testnet();
    testnet_config_peer1.http.port = 0;
    testnet_config_peer1.trusted_peers = trusted_peers.clone();
    testnet_config_peer1.mode = NodeMode::PeerSync;
    let ctx_peer1_node = IrysNodeTest::new_genesis(testnet_config_peer1.clone())
        .await
        .start()
        .await;

    //start peer2
    let ctx_peer2_node = IrysNodeTest::new_genesis(testnet_config_peer1.clone())
        .await
        .start()
        .await;

    // check the height returned by the peers, and when it is high enough do the api call for the block_index and then shutdown the peer
    // this should expand with the block height
    let max_attempts: u64 = required_blocks_height
        .try_into()
        .expect("expected required_blocks_height to be valid u64");
    let max_attempts = max_attempts * 3;

    let result_peer1 = poll_until_fetch_at_block_index_height(
        &ctx_peer1_node.node_ctx,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    // wait and retry hitting the peer_list endpoint of peer1 node
    let peer_list_items =
        poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer1_node.node_ctx).await;
    // assert that peer1 node has updated trusted peers
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    // shut down peer, we have what we need
    ctx_peer1_node.node_ctx.stop().await;

    // wait and retry hitting the peer_list endpoint of peer2 node
    let peer_list_items =
        poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer2_node.node_ctx).await;
    // assert that peer2 node has updated trusted peers
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node.node_ctx,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    // shut down peer, we have what we need
    ctx_peer2_node.node_ctx.stop().await;

    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.http.port),
        0,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;

    //shutdown genesis node, as the peers are no longer going make http calls to it
    ctx_genesis_node.node_ctx.stop().await;

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

fn local_test_url(port: &u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// poll info_endpoint until timeout or we get block_index at desired height
async fn poll_until_fetch_at_block_index_height(
    node_ctx: &IrysNodeCtx,
    required_blocks_height: u64,
    max_attempts: u64,
) -> Option<awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>>> {
    let mut attempts = 0;
    let mut result_peer = None;
    let url = local_test_url(&node_ctx.config.node_config.http.port);
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
                    &local_test_url(&node_ctx.config.node_config.http.port),
                    0,
                    required_blocks_height,
                )
                .await,
            );
        }
    }
    result_peer
}

// poll peer_list_endpoint until timeout or we get the expected result
async fn poll_peer_list(
    trusted_peers: Vec<PeerAddress>,
    node_ctx: &IrysNodeCtx,
) -> Vec<PeerAddress> {
    let mut peer_list_items: Vec<PeerAddress> = Vec::new();
    for _ in 0..20 {
        sleep(Duration::from_millis(2000)).await;

        let mut peer_results_genesis =
            peer_list_endpoint_request(&local_test_url(&node_ctx.config.node_config.http.port))
                .await;

        peer_list_items = peer_results_genesis
            .json::<Vec<PeerAddress>>()
            .await
            .expect("valid PeerAddress");
        peer_list_items.sort(); //sort so we have sane comparisons in asserts
        if &trusted_peers == &peer_list_items {
            break;
        }
    }
    peer_list_items
}
