use crate::api::external_api::{block_index_endpoint_request, info_endpoint_request};
use crate::utils::mine_block;
use irys_actors::BlockFinalizedMessage;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::{IrysNode, IrysNodeCtx};
use irys_testing_utils::utils::{tempfile::TempDir, temporary_directory};
use irys_types::{Address, Config, IrysBlockHeader, IrysTransactionHeader, Signature, H256};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

#[actix_web::test]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let required_blocks_height: usize = 5;
    let trusted_peers = vec!["127.0.0.1:8080".parse().expect("valid SocketAddr expected")];
    let testnet_config_genesis = Config {
        port: 8080,
        trusted_peers: trusted_peers.clone(),
        chunk_size: 32,             // small to allow VERY quick mining of valid blocks
        num_chunks_in_partition: 2, // small to allow VERY quick mining of valid blocks
        ..Config::testnet()
    };
    let ctx_genesis_node = setup_with_config(
        testnet_config_genesis.clone(),
        "heavy_sync_chain_state_genesis",
        true,
        None,
    )
    .await
    .expect("found invalid genesis ctx");

    ctx_genesis_node
        .node
        .actor_addresses
        .start_mining()
        .expect("expected mining to start");

    let genesis_block = Some(ctx_genesis_node.irys_genesis_block);

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
        genesis_block.clone(),
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
        genesis_block.clone(),
    )
    .await
    .expect("found invalid genesis ctx for peer2");

    //FIXME: magic number could be a constant e.g. 3 blocks worth of time?
    sleep(Duration::from_millis(30000)).await; // wait for mining blocks to have occured on genesis node

    // check the height returned by the peers, and when it is high enough do the api call for the block_index and then shutdown the peer
    let max_attempts = 10;

    let result_peer1 = poll_until_fetch_at_block_index_height(
        &ctx_peer1_node,
        required_blocks_height,
        max_attempts,
    )
    .await;

    //shut down peer, we have what we need
    ctx_peer1_node.node.stop().await;

    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node,
        required_blocks_height,
        max_attempts,
    )
    .await;

    //shut down peer, we have what we need
    ctx_peer2_node.node.stop().await;

    let mut result_genesis =
        block_index_endpoint_request(&local_test_url(&testnet_config_genesis.port), 0, 5).await;

    //shutdown genesis node, as the peers are no longer going make http calls to it
    ctx_genesis_node.node.stop().await;

    // compere blocks in indexes from each of the three nodes
    // they should be identical if the sync was a success
    let body_genesis = result_genesis.body().await.expect("expected a valid body");
    let body_peer1 = result_peer1
        .expect("expected a client response from peer1")
        .body()
        .await
        .expect("expected a valid body");
    let body_peer2 = result_peer2
        .expect("expected a client response from peer2")
        .body()
        .await
        .expect("expected a valid body");
    error!("body_genesis {:?}", body_genesis);
    error!("body_peer1   {:?}", body_peer1);
    error!("body_peer2   {:?}", body_peer2);
    assert_eq!(
        body_genesis, body_peer1,
        "expecting body from genesis node {:?} to match body from peer1 {:?}",
        body_genesis, body_peer1
    );
    assert_eq!(
        body_peer1, body_peer2,
        "expecting body from peer1 node {:?} to match body from peer2 {:?}",
        body_peer1, body_peer2
    );

    Ok(())
}

struct TestCtx {
    config: Config,
    node: IrysNodeCtx,
    irys_genesis_block: Arc<IrysBlockHeader>,
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
    genesis_block: Option<Arc<IrysBlockHeader>>,
) -> eyre::Result<TestCtx> {
    let temp_dir = temporary_directory(Some(node_name), false);
    testnet_config.base_directory = temp_dir.path().to_path_buf();
    let mut irys_node = IrysNode::new(testnet_config.clone(), genesis, genesis_block);
    let node = irys_node.init().await?;
    Ok(TestCtx {
        config: testnet_config,
        irys_genesis_block: irys_node.irys_genesis_block,
        node,
        temp_dir,
    })
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
