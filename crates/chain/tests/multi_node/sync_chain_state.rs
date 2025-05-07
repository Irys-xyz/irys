use crate::utils::{mine_blocks, AddTxError, IrysNodeTest};
use alloy_core::primitives::{ruint::aliases::U256, B256};
use irys_actors::mempool_service::TxIngressError;
use irys_api_server::routes::index::NodeInfo;
use irys_chain::{
    peer_utilities::{
        block_index_endpoint_request, info_endpoint_request, peer_list_endpoint_request,
    },
    IrysNodeCtx,
};
use irys_database::{block_header_by_hash, BlockIndexItem};
use irys_types::{
    irys::IrysSigner, Address, Config, GossipConfig, HttpConfig, IrysTransaction, NodeConfig,
    NodeMode, PeerAddress, RethPeerInfo, H256,
};
use k256::ecdsa::SigningKey;
use reth::rpc::{eth::EthApiServer as _, types::engine::PayloadStatusEnum};
use reth::{payload::EthPayloadBuilderAttributes, rpc::types::engine::PayloadAttributes};
use reth_db::Database;
use reth_primitives::irys_primitives::IrysTxId;
use reth_primitives::GenesisAccount;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, Duration};
use tracing::{error, info};

pub(crate) fn eth_payload_attributes(timestamp: u64) -> EthPayloadBuilderAttributes {
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: None, /* Some(B256::ZERO) */
        shadows: None,
    };
    EthPayloadBuilderAttributes::new(B256::ZERO, attributes)
}

#[actix_web::test]
async fn heavy_test_p2p_evm_gossip() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let config = Config::new(NodeConfig::testnet());
    let account1 = IrysSigner::random_signer(&config.consensus);
    let genesis = start_genesis_node(&config.node_config, &account1).await;
    tracing::info!(
        "peer info: {:?}",
        &genesis.node_ctx.config.node_config.reth_peer_info
    );
    let genesis_peer_address = PeerAddress {
        gossip: format!(
            "{}:{}",
            genesis.node_ctx.config.node_config.gossip.bind_ip,
            genesis.node_ctx.config.node_config.gossip.port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        api: format!(
            "{}:{}",
            genesis.node_ctx.config.node_config.http.bind_ip,
            genesis.node_ctx.config.node_config.http.port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        execution: genesis.node_ctx.config.node_config.reth_peer_info,
    };

    let (peer1, peer2) = start_peer_nodes(
        &Config::new(NodeConfig {
            trusted_peers: vec![genesis_peer_address],
            ..NodeConfig::testnet()
        }),
        &Config::new(NodeConfig {
            trusted_peers: vec![genesis_peer_address],
            ..NodeConfig::testnet()
        }),
        &account1,
    )
    .await;

    tracing::info!(
        "genesis: {:?}, peer 1: {:?}, peer 2: {:?}",
        &genesis.node_ctx.config.node_config.reth_peer_info,
        &peer1.node_ctx.config.node_config.reth_peer_info,
        &peer2.node_ctx.config.node_config.reth_peer_info
    );

    // mine_blocks(&genesis.node_ctx, 3).await.unwrap();

    let mut genctx = irys_reth_node_bridge::adapter::node::RethNodeContext::new(
        genesis.node_ctx.reth_handle.clone().into(),
    )
    .await?;

    let p1ctx = irys_reth_node_bridge::adapter::node::RethNodeContext::new(
        peer1.node_ctx.reth_handle.clone().into(),
    )
    .await?;

    // don't use if the reth service connect messages are used
    // genctx.connect(&mut p1ctx).await;
    // p1ctx.connect(&mut genctx).await; <- will fail as it expects to see a new peer session event, and will hang if the peer is already connected

    let (block_hash, block_number) = {
        let (payload, _) = genctx.advance_block(vec![], eth_payload_attributes).await?;
        (payload.block().hash(), payload.block().number)
    };

    genctx.assert_new_block2(block_hash, block_number).await?;

    p1ctx
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    p1ctx.assert_new_block2(block_hash, block_number).await?;

    // sleep(Duration::from_millis(2_000)).await;

    let a2 = p1ctx
        .rpc
        .inner
        .eth_api()
        .block_by_hash(block_hash, false)
        .await?;

    assert!(
        a2.is_some_and(|b| b.header.hash == block_hash),
        "Retrieved blocks hash is correct"
    );

    peer1.stop().await;
    peer2.stop().await;
    genesis.stop().await;

    Ok(())
}

#[actix_web::test]
async fn heavy_test_p2p_evm_gossip_new_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let config = Config::new(NodeConfig::testnet());
    let account1 = IrysSigner::random_signer(&config.consensus);
    let genesis = start_genesis_node(&config.node_config, &account1).await;

    let genesis_peer_address = PeerAddress {
        gossip: format!(
            "{}:{}",
            genesis.node_ctx.config.node_config.gossip.bind_ip,
            genesis.node_ctx.config.node_config.gossip.port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        api: format!(
            "{}:{}",
            genesis.node_ctx.config.node_config.http.bind_ip,
            genesis.node_ctx.config.node_config.http.port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        execution: genesis.node_ctx.config.node_config.reth_peer_info,
    };

    let (peer1, peer2) = start_peer_nodes(
        &Config::new(NodeConfig {
            trusted_peers: vec![genesis_peer_address],
            ..NodeConfig::testnet()
        }),
        &Config::new(NodeConfig {
            trusted_peers: vec![genesis_peer_address],
            ..NodeConfig::testnet()
        }),
        &account1,
    )
    .await;

    info!(
        "genesis: {:?}, peer 1: {:?}, peer 2: {:?}",
        &genesis.node_ctx.config.node_config.reth_peer_info,
        &peer1.node_ctx.config.node_config.reth_peer_info,
        &peer2.node_ctx.config.node_config.reth_peer_info
    );

    // mine_blocks(&genesis.node_ctx, 3).await.unwrap();

    let mut genctx = irys_reth_node_bridge::adapter::node::RethNodeContext::new(
        genesis.node_ctx.reth_handle.clone().into(),
    )
    .await?;

    let p1ctx = irys_reth_node_bridge::adapter::node::RethNodeContext::new(
        peer1.node_ctx.reth_handle.clone().into(),
    )
    .await?;

    // don't use if the reth service connect messages are used
    // genctx.connect(&mut p1ctx).await;
    // p1ctx.connect(&mut genctx).await; <- will fail as it expects to see a new peer session event, and will hang if the peer is already connected

    let (block_hash, block_number) = {
        let p1_latest = genctx
            .rpc
            .inner
            .eth_api()
            .block_by_number(alloy_eips::BlockNumberOrTag::Latest, false)
            .await
            .unwrap()
            .unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let payload_attrs = reth::rpc::types::engine::PayloadAttributes {
            timestamp: now.as_secs(), // tie timestamp together **THIS HAS TO BE SECONDS**
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: None,
            parent_beacon_block_root: None,
            shadows: None,
        };

        let (exec_payload, built, attrs) = genctx
            .new_payload_irys2(p1_latest.header.hash, payload_attrs)
            .await?;

        let block_hash = genctx
            .engine_api
            .submit_payload(
                built.clone(),
                attrs.clone(),
                PayloadStatusEnum::Valid,
                vec![],
            )
            .await?;

        // trigger forkchoice update via engine api to commit the block to the blockchain
        genctx
            .engine_api
            .update_forkchoice(block_hash, block_hash)
            .await?;

        (
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_hash,
            exec_payload
                .execution_payload
                .payload_inner
                .payload_inner
                .payload_inner
                .block_number,
        )
    };

    // assert the block has been committed to the blockchain
    genctx.assert_new_block2(block_hash, block_number).await?;

    // only send forkchoice update to second node
    p1ctx
        .engine_api
        .update_forkchoice(block_hash, block_hash)
        .await?;

    // expect second node advanced via p2p gossip

    p1ctx.assert_new_block2(block_hash, block_number).await?;

    let a2 = p1ctx
        .rpc
        .inner
        .eth_api()
        .block_by_hash(block_hash, false)
        .await?;

    assert!(
        a2.is_some_and(|b| b.header.hash == block_hash),
        "Retrieved blocks hash is correct"
    );

    peer1.stop().await;
    peer2.stop().await;
    genesis.stop().await;

    Ok(())
}

/// 1. spin up a genesis node and two peers. Check that we can sync blocks from the genesis node
/// 2. check that the blocks are valid, check that peer1, peer2, and genesis are indeed synced
/// 3. mine further blocks on genesis node, and confirm gossip service syncs them to peers
/// TODO: Mine on peer2 and see if those blocks arrive at genesis via gossip
#[test_log::test(actix_web::test)]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    // setup trusted peers connection data and configs for genesis and nodes
    let (
        testnet_config_genesis,
        mut testnet_config_peer1,
        testnet_config_peer2,
        trusted_peers,
        genesis_trusted_peers,
    ) = init_configs();
    // setup a funded account at genesis block
    let account1 = IrysSigner::random_signer(&testnet_config_genesis.consensus_config());

    let ctx_genesis_node = start_genesis_node(&testnet_config_genesis, &account1).await;

    let required_blocks_height: usize = 5;
    let expected_block_lag_between_index_and_tree: usize = 2;
    // +2 is so genesis is two blocks ahead of the peer nodes, as currently we check the peers index which lags behind
    let required_genesis_node_height =
        required_blocks_height + expected_block_lag_between_index_and_tree;

    // generate a txn and add it to the block...
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // mine x blocks on genesis
    mine_blocks(&ctx_genesis_node.node_ctx, required_genesis_node_height)
        .await
        .expect("expected many mined blocks");

    // wait and retry hitting the peer_list endpoint of genesis node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_genesis_node).await;
    // assert that genesis node is advertising the trusted peers it was given via config that succeeded in a handshake in trusted_peers_handshake_task()
    // i.e. the only good peer will be genesis at his point in the tests as other peers are not yet online
    // so we expect one peer, and we expect it to have the mining_address of the genesis node
    error!("peer_list_items: {:?}", peer_list_items);
    // assert_eq!(1, peer_list_items.len());
    // assert_eq!(
    //     genesis_trusted_peers[0].api.ip(),
    //     peer_list_items[0].api.ip()
    // );

    testnet_config_peer1.trusted_peers = trusted_peers.clone();

    // start additional nodes (after we have mined some blocks on genesis node)
    let (ctx_peer1_node, ctx_peer2_node) = start_peer_nodes(
        &Config::new(testnet_config_peer1),
        &Config::new(testnet_config_peer2),
        &account1,
    )
    .await;

    let genesis_genesis_block =
        block_header_by_hash(&ctx_genesis_node.node_ctx.db.tx()?, &H256::zero(), false)?.unwrap();

    let peer1_genesis_block =
        block_header_by_hash(&ctx_peer1_node.node_ctx.db.tx()?, &H256::zero(), false)?.unwrap();

    let peer2_genesis_block =
        block_header_by_hash(&ctx_peer2_node.node_ctx.db.tx()?, &H256::zero(), false)?.unwrap();

    assert!(genesis_genesis_block == peer1_genesis_block);
    assert!(genesis_genesis_block == peer2_genesis_block);

    // check the height returned by the peers, and when it is high enough do the api call for the block_index and then shutdown the peer
    // this should expand with the block height
    let max_attempts: u64 = required_blocks_height
        .try_into()
        .expect("expected required_blocks_height to be valid u64");
    let max_attempts = max_attempts * 3;

    let result_peer1 = poll_until_fetch_at_block_index_height(
        "peer1".to_owned(),
        &ctx_peer1_node.node_ctx,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    // wait and retry hitting the peer_list endpoint of peer1 node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer1_node).await;
    // assert that peer1 node has updated trusted peers, - 1 because we shouldn't add ourselves
    assert_eq!(peer_list_items.len(), genesis_trusted_peers.len() - 1);

    // wait and retry hitting the peer_list endpoint of peer2 node
    let peer_list_items = poll_peer_list(genesis_trusted_peers.clone(), &ctx_peer2_node).await;
    // assert that peer2 node has updated trusted peers
    assert_eq!(peer_list_items.len(), genesis_trusted_peers.len() - 1);

    let result_peer2 = poll_until_fetch_at_block_index_height(
        "peer2".to_owned(),
        &ctx_peer2_node.node_ctx,
        required_blocks_height
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        max_attempts,
    )
    .await;

    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.http.port),
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

    tracing::debug!("STARTUP SEQUENCE ASSERTS WERE A SUCCESS. TO GET HERE TAKES ~2 MINUTES");

    /*
    // BEGIN TESTING BLOCK GOSSIP FROM PEER2 to GENESIS
     */

    //TEST: generate a txn on peer2, and then continue mining on genesis to see if the txn is picked up in the next block via gossip
    let txn = generate_test_transaction_and_add_to_block(&ctx_peer2_node, &account1).await;
    tracing::debug!("txn we are looking for on genesis: {:?}", txn);

    // mine block on genesis
    mine_blocks(&ctx_genesis_node.node_ctx, 1)
        .await
        .expect("expected one mined block on genesis node");

    let result_genesis = poll_until_fetch_at_block_index_height(
        "genesis".to_owned(),
        &ctx_genesis_node.node_ctx,
        (required_blocks_height + 1)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        20,
    )
    .await;
    let result_peer2 = poll_until_fetch_at_block_index_height(
        "peer2".to_owned(),
        &ctx_peer2_node.node_ctx,
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

    tracing::debug!("block_index_genesis: {:?}", block_index_genesis);
    tracing::debug!("block_index_peer2: {:?}", block_index_peer2);

    assert_eq!(
        block_index_genesis, block_index_peer2,
        "expecting json from genesis node {:?} to match json from peer2 {:?}",
        block_index_genesis, block_index_peer2
    );

    // mine more blocks on peer2 node, and see if gossip service brings them to genesis
    /*let additional_blocks_for_gossip_test: usize = 2;
    //wait 10 seconds to ensure vdf has moved forward on peer2, otherwise we get an error: Block validation error Unavailable requested range (16..=20). Stored steps range is (1..=19)
    sleep(Duration::from_millis(10000)).await;
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
        &local_test_url(&testnet_config_peer2.api_port),
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
    );*/

    /*
    // BEGIN TESTING BLOCK GOSSIP FROM GENESIS to PEER2
     */

    tracing::debug!("BEGIN TESTING BLOCK GOSSIP FROM GENESIS to PEER2");

    // mine more blocks on genesis node, and see if gossip service brings them to peer2
    let additional_blocks_for_gossip_test: usize = 2;
    tracing::debug!("MINING BLOCKS ON GENESIS TO BE GOSIPPED");
    mine_blocks(
        &ctx_genesis_node.node_ctx,
        additional_blocks_for_gossip_test,
    )
    .await
    .expect("expected many mined blocks");

    // TODO WE could possibly add a check here to see if genesis really did mine the block and the index height has increased...
    let mut result_genesis = block_index_endpoint_request(
        &local_test_url(&testnet_config_genesis.http.port),
        0,
        (required_blocks_height + 1 + additional_blocks_for_gossip_test)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
    )
    .await;

    let result_peer1 = poll_until_fetch_at_block_index_height(
        "peer1".to_owned(),
        &ctx_peer1_node.node_ctx,
        (required_blocks_height + 1 + additional_blocks_for_gossip_test)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        2000,
    )
    .await;

    //now see if the block makes its way to peer2 via gossip service
    let result_peer2 = poll_until_fetch_at_block_index_height(
        "peer2".to_owned(),
        &ctx_peer2_node.node_ctx,
        (required_blocks_height + 1 + additional_blocks_for_gossip_test)
            .try_into()
            .expect("expected required_blocks_height to be valid u64"),
        2000,
    )
    .await;

    tracing::debug!("PEER2 should have got the block");

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
        block_index_genesis, block_index_peer2,
        "expecting json from genesis node {:?} to match json from peer2 {:?}",
        block_index_genesis, block_index_peer2
    );

    tracing::debug!("COMPLETED FINAL PEER2 ASSERTS");

    // shut down peer nodes and then genesis node, we have what we need
    ctx_peer1_node.stop().await;
    ctx_peer2_node.stop().await;
    ctx_genesis_node.stop().await;

    Ok(())
}

/// setup configs for genesis, peer1 and peer2 for e2e tests
fn init_configs() -> (
    NodeConfig,
    NodeConfig,
    NodeConfig,
    Vec<PeerAddress>,
    Vec<PeerAddress>,
) {
    let mut testnet_config_genesis = NodeConfig {
        http: HttpConfig {
            port: 8080,
            bind_ip: "127.0.0.1".to_string(),
        },
        gossip: GossipConfig {
            port: 8081,
            bind_ip: "127.0.0.1".to_string(),
        },
        mining_key: SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key"),
        mode: NodeMode::Genesis,
        ..NodeConfig::testnet()
    };
    let mut testnet_config_peer1 = NodeConfig {
        http: HttpConfig {
            // Use random port
            port: 0,
            bind_ip: "127.0.0.1".to_string(),
        },
        gossip: GossipConfig {
            port: 8083,
            bind_ip: "127.0.0.1".to_string(),
        },
        mining_key: SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d1")
                .expect("valid hex"),
        )
        .expect("valid key"),
        mode: NodeMode::PeerSync,
        ..NodeConfig::testnet()
    };
    let mut testnet_config_peer2 = NodeConfig {
        http: HttpConfig {
            port: 0,
            bind_ip: "127.0.0.1".to_string(),
        },
        gossip: GossipConfig {
            port: 8085,
            bind_ip: "127.0.0.1".to_string(),
        },
        mining_key: SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d2")
                .expect("valid hex"),
        )
        .expect("valid key"),
        mode: NodeMode::PeerSync,
        ..NodeConfig::testnet()
    };
    let trusted_peers = vec![PeerAddress {
        api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
        gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
        execution: RethPeerInfo::default(),
    }];
    let genesis_trusted_peers = vec![
        PeerAddress {
            api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        },
        PeerAddress {
            api: "127.0.0.1:8082".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.1:8083".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        },
        PeerAddress {
            api: "127.0.0.1:8084".parse().expect("valid SocketAddr expected"),
            gossip: "127.0.0.1:8085".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        },
    ];
    testnet_config_peer1.trusted_peers = trusted_peers.clone();
    testnet_config_peer2.trusted_peers = trusted_peers.clone();
    testnet_config_genesis.trusted_peers = genesis_trusted_peers.clone();
    (
        testnet_config_genesis,
        testnet_config_peer1,
        testnet_config_peer2,
        trusted_peers,
        genesis_trusted_peers,
    )
}

/// add a single account to the supplied node config
fn add_account_to_config(irys_node_config: &mut NodeConfig, account: &IrysSigner) -> () {
    irys_node_config.consensus.extend_genesis_accounts(vec![(
        account.address(),
        GenesisAccount {
            balance: U256::from(1000),
            ..Default::default()
        },
    )]);
}

/// start genesis node with an account
async fn start_genesis_node(
    testnet_config_genesis: &NodeConfig,
    account: &IrysSigner, // account with balance at genesis
) -> IrysNodeTest<IrysNodeCtx> {
    // init genesis node
    let mut genesis_node = IrysNodeTest::new_genesis(testnet_config_genesis.clone()).await;
    // add accounts with balances to genesis node
    add_account_to_config(&mut genesis_node.cfg, &account);
    // start genesis node
    let ctx_genesis_node = genesis_node.start().await;
    ctx_genesis_node
}

/// start peer nodes with an account
async fn start_peer_nodes(
    testnet_config_peer1: &Config,
    testnet_config_peer2: &Config,
    account: &IrysSigner, // account with balance at genesis
) -> (IrysNodeTest<IrysNodeCtx>, IrysNodeTest<IrysNodeCtx>) {
    let mut peer1_node = IrysNodeTest::new(testnet_config_peer1.node_config.clone()).await;
    add_account_to_config(&mut peer1_node.cfg, &account);
    let ctx_peer1_node = peer1_node.start().await;
    let mut peer2_node = IrysNodeTest::new(testnet_config_peer2.node_config.clone()).await;
    add_account_to_config(&mut peer2_node.cfg, &account);
    let ctx_peer2_node = peer2_node.start().await;
    (ctx_peer1_node, ctx_peer2_node)
}

/// helper function to reduce replication of local ip in codebase
fn local_test_url(port: &u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// generate a test transaction, submit it to be added to mempool, return txn hashmap
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
    node_name: String,
    node_ctx: &IrysNodeCtx,
    required_blocks_height: u64,
    max_attempts: u64,
) -> Option<awc::ClientResponse<actix_web::dev::Decompress<actix_http::Payload>>> {
    let mut attempts = 0;
    let mut result_peer = None;
    let max_attempts = max_attempts * 10;
    let url = local_test_url(&node_ctx.config.node_config.http.port);
    loop {
        let mut response = info_endpoint_request(&url).await;

        if max_attempts < attempts {
            error!(
                "{} never fully synced to height {}",
                node_name, required_blocks_height
            );
            break;
        } else {
            attempts += 1;
        }

        let json_response: NodeInfo = response.json().await.expect("valid NodeInfo");
        if required_blocks_height > json_response.block_index_height {
            tracing::debug!(
                "{} attempt {} checking {}. required_blocks_height > json_response.block_index_height {} > {}",
                node_name, &attempts, &url, required_blocks_height, json_response.block_index_height
            );
            //wait one second and try again
            sleep(Duration::from_millis(100)).await;
        } else {
            result_peer = Some(
                block_index_endpoint_request(
                    &local_test_url(&node_ctx.config.node_config.http.port),
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

/// poll peer_list_endpoint until timeout or we get the expected result
async fn poll_peer_list(
    trusted_peers: Vec<PeerAddress>,
    ctx_node: &IrysNodeTest<IrysNodeCtx>,
) -> Vec<PeerAddress> {
    let mut peer_list_items: Vec<PeerAddress> = Vec::new();
    let max_attempts = 200;
    for _ in 0..max_attempts {
        sleep(Duration::from_millis(100)).await;

        let mut peer_results_genesis = peer_list_endpoint_request(&local_test_url(
            &ctx_node.node_ctx.config.node_config.http.port,
        ))
        .await;

        peer_list_items = peer_results_genesis
            .json::<Vec<PeerAddress>>()
            .await
            .expect("valid PeerAddress");
        peer_list_items.sort(); //sort peer list so we have sane comparisons in asserts
        if &trusted_peers == &peer_list_items {
            break;
        }
    }
    peer_list_items
}
