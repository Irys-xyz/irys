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
        None,
    )
    .await
    .expect("found invalid genesis ctx");

    // start mining
    // advance one block
    let (_header, _payload) = mine_block(&ctx_genesis_node.node).await?.unwrap();
    // advance one block, finalizing the previous block
    let (header, _payload) = mine_block(&ctx_genesis_node.node).await?.unwrap();
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
        chain_id: ctx_genesis_node.config.chain_id,
        version: 0,
        ingress_proofs: None,
        signature: Signature::test_signature().into(),
    };
    let block_finalized_message = BlockFinalizedMessage {
        block_header: header,
        all_txs: Arc::new(vec![mock_header]),
    };
    sleep(Duration::from_millis(10000)).await;

    let _ = ctx_genesis_node
        .node
        .actor_addresses
        .block_index
        .send(block_finalized_message)
        .await
        .expect("expected valid response from block_index actor");

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
    sleep(Duration::from_millis(10000)).await;

    let mut result_genesis =
        block_index_endpoint_request(&local_test_url(&testnet_config_genesis.port), 0, 5).await;

    // check the height returned by the peers, and when it is high enough do the api call for the block_index and then shutdown the peer
    let required_blocks_height = 2;
    let max_attempts = 20;

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
    loop {
        let mut response = info_endpoint_request(&local_test_url(&node_ctx.node.config.port)).await;

        if max_attempts < attempts {
            error!("peer never fully synced");
            break;
        } else {
            attempts += 1;
        }

        let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");
        if required_blocks_height > json_response.block_index_height {
            debug!(
                "attempt {}. required_blocks_height > json_response.block_index_height {} > {}",
                &attempts, required_blocks_height, json_response.block_index_height
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
