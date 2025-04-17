use crate::utils::{mine_blocks, AddTxError, IrysNodeTest};
use alloy_core::primitives::ruint::aliases::U256;
use irys_actors::mempool_service::TxIngressError;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::{
    peer_utilities::{
        block_index_endpoint_request, info_endpoint_request, peer_list_endpoint_request,
    },
    IrysNodeCtx,
};
use irys_config::IrysNodeConfig;
use irys_database::BlockIndexItem;
use irys_types::{irys::IrysSigner, Config, IrysTransaction, PeerAddress};
use reth_primitives::irys_primitives::IrysTxId;
use reth_primitives::GenesisAccount;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

/// 1. spin up a genesis node and two peers. Check that we can sync blocks from the genesis node
/// 2. check that the blocks are valid, check that peer1, peer2, and genesis are indeed synced
/// 3. mine further blocks on genesis node, and confirm gossip service syncs them to peers
#[test_log::test(actix_web::test)]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    // setup trusted peers connection data
    let (trusted_peers, genesis_trusted_peers) = init_peers();
    // setup configs for genesis and nodes
    let (testnet_config_genesis, testnet_config_peer1, testnet_config_peer2) =
        init_configs(&genesis_trusted_peers, &trusted_peers);
    // setup a funded account at genesis block
    let account1 = IrysSigner::random_signer(&testnet_config_genesis);

    let ctx_genesis_node = start_genesis_node(&testnet_config_genesis, &account1).await;

    let required_blocks_height: usize = 5;
    // +2 is so genesis is two blocks ahead of the peer nodes, as currently we check the peers index which lags behind
    let required_genesis_node_height = required_blocks_height + 2;

    // generate a txn and add it to the block...
    sleep(Duration::from_millis(5000)).await; //wait before trying that
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // mine x blocks on genesis
    mine_blocks(&ctx_genesis_node.node_ctx, required_genesis_node_height)
        .await
        .expect("expected many mined blocks");

    // wait and retry hitting the peer_list endpoint of genesis node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_genesis_node).await;
    // assert that genesis node is advertising the trusted peers it was given via config
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    // start additional nodes (after we have mined some blocks on genesis node)
    let (ctx_peer1_node, ctx_peer2_node) =
        start_peer_nodes(&testnet_config_peer1, &testnet_config_peer2, &account1).await;

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

    // wait and retry hitting the peer_list endpoint of peer1 node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer1_node).await;
    // assert that peer1 node has updated trusted peers
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    // wait and retry hitting the peer_list endpoint of peer2 node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer2_node).await;
    // assert that peer2 node has updated trusted peers
    assert_eq!(&genesis_trusted_peers, &peer_list_items);

    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.port),
        0,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;

    // compare blocks in indexes from each of the three nodes
    // they should be identical if the startup sync was a success
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

    debug!("STARTUP SEQUENCE ASSERTS WERE A SUCCESS. TO GET HERE TAKES ~2 MINUTES");

    /*
    // BEGIN TESTING BLOCK GOSSIP FROM PEER2 to GENESIS
     */

    //TEST: generate a txn on peer2, and then continue mining on genesis to see if the txn is picked up in the next block via gossip
    let txn = generate_test_transaction_and_add_to_block(&ctx_peer2_node, &account1).await;
    error!("txn we are looking for on genesis: {:?}", txn);

    sleep(Duration::from_millis(5000)).await;
    // mine block on genesis
    mine_blocks(&ctx_genesis_node.node_ctx, 1)
        .await
        .expect("expected one mined block on genesis node");

    sleep(Duration::from_millis(1000000)).await;

    let result_genesis = poll_until_fetch_at_block_index_height(
        &ctx_genesis_node,
        (required_blocks_height + 1)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        20,
    )
    .await;
    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node,
        (required_blocks_height + 1)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        20,
    )
    .await;

    let block_index_genesis = result_genesis
        .expect("expected a client response from genesis")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    let block_index_peer2 = result_peer2
        .expect("expected a client response from peer2")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");

    error!("block_index_genesis: {:?}", block_index_genesis);
    error!("block_index_peer2: {:?}", block_index_peer2);

    assert_eq!(
        block_index_genesis, block_index_peer2,
        "expecting json from genesis node {:?} to match json from peer2 {:?}",
        block_index_genesis, block_index_peer2
    );

    // mine more blocks on peer2 node, and see if gossip service brings them to genesis
    let additional_blocks_for_gossip_test: usize = 2;
    mine_blocks(&ctx_peer2_node.node_ctx, additional_blocks_for_gossip_test)
        .await
        .expect("expected many mined blocks");
    let result_genesis = poll_until_fetch_at_block_index_height(
        &ctx_genesis_node,
        (required_blocks_height + additional_blocks_for_gossip_test)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        20,
    )
    .await;

    let mut result_peer2 = block_index_endpoint_request(
        &local_test_url(&testnet_config_peer2.port),
        0,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;
    let block_index_genesis = result_genesis
        .expect("expected a client response from peer2")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    let block_index_peer2 = result_peer2
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    assert_eq!(
        block_index_genesis, block_index_peer2,
        "expecting json from genesis node {:?} to match json from peer2 {:?}",
        block_index_genesis, block_index_peer2
    );

    /*
    // BEGIN TESTING BLOCK GOSSIP FROM GENESIS to PEER2
     */

    // mine more blocks on genesis node, and see if gossip service brings them to peer2
    /*let additional_blocks_for_gossip_test: usize = 2;
    mine_blocks(
        &ctx_genesis_node.node_ctx,
        additional_blocks_for_gossip_test,
    )
    .await
    .expect("expected many mined blocks");
    let result_peer2 = poll_until_fetch_at_block_index_height(
        &ctx_peer2_node,
        (required_blocks_height + additional_blocks_for_gossip_test)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        20,
    )
    .await;

    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.port),
        0,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;
    let block_index_genesis = result_genesis
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    let block_index_peer2 = result_peer2
        .expect("expected a client response from peer2")
        .json::<Vec<BlockIndexItem>>()
        .await
        .expect("expected a valid json deserialize");
    assert_eq!(
        block_index_genesis, block_index_peer2,
        "expecting json from genesis node {:?} to match json from peer2 {:?}",
        block_index_genesis, block_index_peer2
    );*/

    // shut down peer nodes and then genesis node, we have what we need
    ctx_peer1_node.stop().await;
    ctx_peer2_node.stop().await;
    ctx_genesis_node.stop().await;

    Ok(())
}

fn init_peers() -> (Vec<PeerAddress>, Vec<PeerAddress>) {
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
    (trusted_peers, genesis_trusted_peers)
}

fn init_configs(
    genesis_trusted_peers: &Vec<PeerAddress>,
    trusted_peers: &Vec<PeerAddress>,
) -> (Config, Config, Config) {
    let testnet_config_genesis = Config {
        port: 8080,
        gossip_service_port: 8081,
        trusted_peers: genesis_trusted_peers.clone(),
        ..Config::testnet()
    };
    let testnet_config_peer1 = Config {
        port: 0, //random port
        trusted_peers: trusted_peers.clone(),
        ..Config::testnet()
    };
    let testnet_config_peer2 = Config {
        port: 0, //random port
        trusted_peers: trusted_peers.clone(),
        ..Config::testnet()
    };
    (
        testnet_config_genesis,
        testnet_config_peer1,
        testnet_config_peer2,
    )
}

fn add_account_to_config(irys_node_config: &mut IrysNodeConfig, account: &IrysSigner) -> () {
    irys_node_config.extend_genesis_accounts(vec![(
        account.address(),
        GenesisAccount {
            balance: U256::from(1000),
            ..Default::default()
        },
    )]);
}

async fn start_genesis_node(
    testnet_config_genesis: &Config,
    account: &IrysSigner, // account with balance at genesis
) -> IrysNodeTest<IrysNodeCtx> {
    // init genesis node
    let mut genesis_node = IrysNodeTest::new_genesis(testnet_config_genesis.clone()).await;
    // add accounts with balances to genesis node
    add_account_to_config(&mut genesis_node.cfg.irys_node_config, &account);
    // start genesis node
    let ctx_genesis_node = genesis_node.start().await;
    ctx_genesis_node
}

async fn start_peer_nodes(
    testnet_config_peer1: &Config,
    testnet_config_peer2: &Config,
    account: &IrysSigner, // account with balance at genesis
) -> (IrysNodeTest<IrysNodeCtx>, IrysNodeTest<IrysNodeCtx>) {
    let mut peer1_node = IrysNodeTest::new(testnet_config_peer1.clone()).await;
    add_account_to_config(&mut peer1_node.cfg.irys_node_config, &account);
    let ctx_peer1_node = peer1_node.start().await;
    let mut peer2_node = IrysNodeTest::new(testnet_config_peer2.clone()).await;
    add_account_to_config(&mut peer2_node.cfg.irys_node_config, &account);
    let ctx_peer2_node = peer2_node.start().await;
    (ctx_peer1_node, ctx_peer2_node)
}

fn local_test_url(port: &u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

async fn generate_test_transaction_and_add_to_block(
    node: &IrysNodeTest<IrysNodeCtx>,
    account: &IrysSigner,
) -> HashMap<IrysTxId, irys_types::IrysTransaction> {
    let data_bytes = "Test transaction!".as_bytes().to_vec();
    let mut irys_txs: HashMap<IrysTxId, IrysTransaction> = HashMap::new();
    match node.create_submit_data_tx(&account, data_bytes).await {
        Ok(tx) => {
            irys_txs.insert(IrysTxId::from_slice(tx.header.id.as_bytes()), tx);
        }
        Err(AddTxError::TxIngress(TxIngressError::Unfunded)) => {
            panic!("unfunded account error")
        }
        Err(e) => panic!("unexpected error {:?}", e),
    }
    irys_txs
}

/// poll info_endpoint until timeout or we get block_index at desired height
async fn poll_until_fetch_at_block_index_height(
    node_ctx: &IrysNodeTest<IrysNodeCtx>,
    required_blocks_height: u64,
    max_attempts: u64,
) -> Option<awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>>> {
    let mut attempts = 0;
    let mut result_peer = None;
    let url = local_test_url(&node_ctx.node_ctx.config.port);
    loop {
        let mut response = info_endpoint_request(&url).await;

        if max_attempts < attempts {
            error!(
                "peer never fully synced to height {}",
                required_blocks_height
            );
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
            //wait one second and try again
            sleep(Duration::from_millis(1000)).await;
        } else {
            result_peer = Some(
                block_index_endpoint_request(
                    &local_test_url(&node_ctx.node_ctx.config.port),
                    0,
                    required_blocks_height,
                )
                .await,
            );
            break;
        }
    }
    result_peer
}

// poll peer_list_endpoint until timeout or we get the expected result
async fn poll_peer_list(
    trusted_peers: Vec<PeerAddress>,
    ctx_node: &IrysNodeTest<IrysNodeCtx>,
) -> Vec<PeerAddress> {
    let mut peer_list_items: Vec<PeerAddress> = Vec::new();
    for _ in 0..20 {
        sleep(Duration::from_millis(2000)).await;

        let mut peer_results_genesis =
            peer_list_endpoint_request(&local_test_url(&ctx_node.node_ctx.config.port)).await;

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
