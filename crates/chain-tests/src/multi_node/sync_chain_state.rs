use crate::utils::{AddTxError, IrysNodeTest};
use irys_actors::mempool_service::TxIngressError;

use irys_chain::{
    peer_utilities::{block_index_endpoint_request, info_endpoint_request},
    IrysNodeCtx,
};
use irys_database::block_header_by_hash;
use irys_types::{
    irys::IrysSigner, BlockIndexItem, DataTransaction, IrysTransactionId, NodeConfig, NodeInfo,
    NodeMode, SyncMode,
};
use reth::rpc::types::BlockNumberOrTag;
use reth_db::Database as _;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info};

#[test_log::test(tokio::test)]
async fn heavy3_test_p2p_reth_gossip() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    reth_tracing::init_test_tracing();
    let mut genesis_config = NodeConfig::testing();
    let peer_account = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_account]);

    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis.testing_peer_with_signer(&peer_account);
    let peer1 = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER1")
        .await;
    let peer2 = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER2")
        .await;

    tracing::info!("peer info: {:?}", &genesis.node_ctx.config.node_config.reth);

    tracing::info!(
        "genesis: {:?}, peer 1: {:?}, peer 2: {:?}",
        &genesis.node_ctx.config.node_config.reth,
        &peer1.node_ctx.config.node_config.reth,
        &peer2.node_ctx.config.node_config.reth
    );

    // mine_blocks(&genesis.node_ctx, 3).await.unwrap();

    let mut genctx = genesis.node_ctx.reth_node_adapter.clone();
    let p1ctx = peer1.node_ctx.reth_node_adapter.clone();

    // don't use if the reth service connect messages are used
    // genctx.connect(&mut p1ctx).await;
    // p1ctx.connect(&mut genctx).await; <- will fail as it expects to see a new peer session event, and will hang if the peer is already connected

    let (block_hash, block_number) = {
        // make the node advance
        let payload = genctx.advance_block_testing().await?;

        (payload.block().hash(), payload.block().number)
    };

    genctx
        .assert_new_block_irys(block_hash, block_number)
        .await?;

    p1ctx.update_forkchoice(block_hash, block_hash).await?;

    p1ctx
        .assert_new_block_irys(block_hash, block_number)
        .await?;

    // sleep(Duration::from_millis(2_000)).await;

    let a2 = peer1
        .wait_for_reth_marker(BlockNumberOrTag::Number(block_number), block_hash, 10)
        .await?;

    assert_eq!(a2, block_hash, "Retrieved block hash should match expected");

    peer1.stop().await;
    peer2.stop().await;
    genesis.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy3_test_p2p_evm_gossip_new_rpc() -> eyre::Result<()> {
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing();
    let peer_account = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_account]);

    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis.testing_peer_with_signer(&peer_account);
    let peer1 = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER1")
        .await;
    let peer2 = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER2")
        .await;

    info!(
        "genesis: {:?}, peer 1: {:?}, peer 2: {:?}",
        &genesis.node_ctx.config.node_config.reth,
        &peer1.node_ctx.config.node_config.reth,
        &peer2.node_ctx.config.node_config.reth
    );

    // mine_blocks(&genesis.node_ctx, 3).await.unwrap();

    let mut genctx = genesis.node_ctx.reth_node_adapter.clone();
    let p1ctx = peer1.node_ctx.reth_node_adapter.clone();

    // don't use if the reth service connect messages are used
    // genctx.connect(&mut p1ctx).await;
    // p1ctx.connect(&mut genctx).await; <- will fail as it expects to see a new peer session event, and will hang if the peer is already connected

    let (block_hash, block_number) = {
        let built = genctx.advance_block_testing().await?;

        (built.block().hash(), built.block().number)
    };

    // assert the block has been committed to the blockchain
    genctx
        .assert_new_block_irys(block_hash, block_number)
        .await?;

    // only send forkchoice update to second node
    p1ctx.update_forkchoice(block_hash, block_hash).await?;

    // expect second node advanced via p2p gossip

    p1ctx
        .assert_new_block_irys(block_hash, block_number)
        .await?;

    let a2 = peer1
        .wait_for_reth_marker(BlockNumberOrTag::Number(block_number), block_hash, 10)
        .await?;

    assert_eq!(a2, block_hash, "Retrieved block hash should match expected");

    peer1.stop().await;
    peer2.stop().await;
    genesis.stop().await;

    Ok(())
}

/// 1. spin up a genesis node and two peers. Check that we can sync blocks from the genesis node
/// 2. check that the blocks are valid, check that peer1, peer2, and genesis are indeed synced
/// 3. mine further blocks on genesis node, and confirm gossip service syncs them to peers
#[test_log::test(tokio::test)]
async fn heavy4_sync_chain_state_then_gossip_blocks() -> eyre::Result<()> {
    let required_index_blocks_height: usize = 2;
    let max_seconds = 20;

    // setup trusted peers connection data and configs for genesis and nodes
    let mut testing_config_genesis = NodeConfig::testing();
    testing_config_genesis
        .consensus
        .get_mut()
        .mempool
        .tx_anchor_expiry_depth = 20;

    testing_config_genesis
        .consensus
        .get_mut()
        .block_migration_depth = 4;

    let account1 = testing_config_genesis.signer();

    let ctx_genesis_node = IrysNodeTest::new_genesis(testing_config_genesis.clone())
        .start_and_wait_for_packing("GENESIS", max_seconds)
        .await;

    // retrieve block_migration_depth for use later
    let mut consensus = ctx_genesis_node.cfg.consensus.clone();
    let block_migration_depth: usize = consensus.get_mut().block_migration_depth.try_into()?;

    // the x2 is to ensure the block index of the peers contains the blocks we need
    // if this was left as block_migration_depth, because the index endpoint is used
    // on genesis node to sync to the peers, they lag behind by block_migration_depth
    let required_genesis_node_height: usize =
        required_index_blocks_height + (block_migration_depth * 2);

    // generate a txn and add it to the block...
    generate_test_transaction_and_add_to_block(&ctx_genesis_node, &account1).await;

    // mine x blocks on genesis
    ctx_genesis_node
        .mine_blocks(required_genesis_node_height)
        .await
        .expect("expected many mined blocks");
    // wait for block tree
    ctx_genesis_node
        .wait_until_height(required_genesis_node_height.try_into()?, max_seconds)
        .await?;
    // wait for block index
    ctx_genesis_node
        .wait_until_block_index_height(required_index_blocks_height.try_into()?, max_seconds)
        .await?;

    // start additional nodes (after we have mined some blocks on genesis node)
    let ctx_peer1_node = ctx_genesis_node.testing_peer();
    let ctx_peer1_node = IrysNodeTest::new(ctx_peer1_node.clone())
        .start_with_name("PEER1")
        .await;

    let mut ctx_peer2_node = ctx_genesis_node.testing_peer();
    ctx_peer2_node.node_mode = NodeMode::Peer;
    ctx_peer2_node.consensus.get_mut().expected_genesis_hash =
        Some(ctx_genesis_node.node_ctx.genesis_hash);
    ctx_peer2_node.sync_mode = SyncMode::Trusted;
    let ctx_peer2_node = IrysNodeTest::new(ctx_peer2_node.clone())
        .start_with_name("PEER2")
        .await;

    // disable vdf mining on the peers, as they can instead use VDF fast forward as blocks arrive
    // this does not directly contribute to the test but does reduce resource usage during test run
    ctx_peer1_node
        .node_ctx
        .set_partition_mining(false)
        .expect("expect setting mining false on peer1");
    ctx_peer2_node
        .node_ctx
        .set_partition_mining(false)
        .expect("expect setting mining false on peer2");

    //
    // TEST CASE: check genesis blocks match across the three nodes
    //
    {
        // Compare actual genesis block headers across nodes using the proper genesis hash
        let genesis_genesis_block = block_header_by_hash(
            &ctx_genesis_node.node_ctx.db.tx()?,
            &ctx_genesis_node.node_ctx.genesis_hash,
            false,
        )?
        .unwrap();

        let peer1_genesis_block = block_header_by_hash(
            &ctx_peer1_node.node_ctx.db.tx()?,
            &ctx_peer1_node.node_ctx.genesis_hash,
            false,
        )?
        .unwrap();

        let peer2_genesis_block = block_header_by_hash(
            &ctx_peer2_node.node_ctx.db.tx()?,
            &ctx_peer2_node.node_ctx.genesis_hash,
            false,
        )?
        .unwrap();

        assert_eq!(genesis_genesis_block, peer1_genesis_block);
        assert_eq!(genesis_genesis_block, peer2_genesis_block);
    }

    //
    // TEST CASE: check nodes can all see each other in peer lists
    //
    {
        // wait for peer1 block tree
        ctx_peer1_node
            .wait_until_height(required_index_blocks_height.try_into()?, max_seconds)
            .await?;

        // check the height returned by the peers, and when it is high enough,
        // do the api call for the block_index and then shutdown the peer
        let result_peer1 = poll_until_fetch_at_block_index_height(
            "peer1".to_owned(),
            &ctx_peer1_node.node_ctx,
            required_index_blocks_height.try_into()?,
            max_seconds.try_into()?,
        )
        .await;

        // Proactively announce peers to each other to converge peer lists deterministically
        IrysNodeTest::announce_between(&ctx_peer1_node, &ctx_genesis_node)
            .await
            .expect("peer1 <-> genesis handshake");
        IrysNodeTest::announce_between(&ctx_peer2_node, &ctx_genesis_node)
            .await
            .expect("peer2 <-> genesis handshake");
        IrysNodeTest::announce_between(&ctx_peer1_node, &ctx_peer2_node)
            .await
            .expect("peer1 <-> peer2");

        // Check peer lists - each peer should see the genesis node and the other peer

        // Get the peer addresses for comparison
        let genesis_peer_addr = ctx_genesis_node.node_ctx.config.node_config.peer_address();
        let peer1_peer_addr = ctx_peer1_node.node_ctx.config.node_config.peer_address();
        let peer2_peer_addr = ctx_peer2_node.node_ctx.config.node_config.peer_address();

        // Wait until all peers are mutually visible
        ctx_peer1_node
            .wait_until_sees_peer(&genesis_peer_addr, 400)
            .await
            .expect("Peer1 should see genesis node");
        ctx_peer1_node
            .wait_until_sees_peer(&peer2_peer_addr, 400)
            .await
            .expect("Peer1 should see peer2");
        ctx_peer2_node
            .wait_until_sees_peer(&genesis_peer_addr, 400)
            .await
            .expect("Peer2 should see genesis node");
        ctx_peer2_node
            .wait_until_sees_peer(&peer1_peer_addr, 400)
            .await
            .expect("Peer2 should see peer1");

        let result_peer2 = poll_until_fetch_at_block_index_height(
            "peer2".to_owned(),
            &ctx_peer2_node.node_ctx,
            required_index_blocks_height.try_into()?,
            max_seconds.try_into()?,
        )
        .await;

        let result_genesis = block_index_endpoint_request(
            &local_test_url(&ctx_genesis_node.node_ctx.config.node_config.http.bind_port),
            0,
            required_index_blocks_height.try_into()?,
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

        tracing::debug!("startup sequence asserts were a success");
    }

    //
    // TEST CASE: generate a txn on peer2, and then continue mining on genesis to see
    //            if the txn is picked up in the next genesis node block via gossip
    {
        if let Some((_, submit_tx)) =
            generate_test_transaction_and_add_to_block(&ctx_peer2_node, &account1)
                .await
                .iter()
                .next()
        {
            tracing::debug!("txn we are looking for on genesis: {:?}", submit_tx);

            //wait for txn to be gossiped to genesis node
            ctx_genesis_node
                .wait_for_mempool(submit_tx.header.id, max_seconds)
                .await?;
        }

        let genesis_index_height = ctx_genesis_node.get_block_index_height();
        // mine block on genesis
        ctx_genesis_node
            .mine_blocks(1)
            .await
            .expect("expected one mined block on genesis node");

        let result_genesis = poll_until_fetch_at_block_index_height(
            "genesis".to_owned(),
            &ctx_genesis_node.node_ctx,
            genesis_index_height + 1,
            20,
        )
        .await;
        let result_peer2 = poll_until_fetch_at_block_index_height(
            "peer2".to_owned(),
            &ctx_peer2_node.node_ctx,
            genesis_index_height + 1,
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
    }

    //
    // TEST CASE: mine more blocks on genesis node, and see if gossip service brings them to peer2
    //
    {
        let additional_blocks_for_gossip_test: usize = 2;
        let genesis_starting_index_height = ctx_genesis_node.get_block_index_height();
        ctx_genesis_node
            .mine_blocks(additional_blocks_for_gossip_test)
            .await
            .expect("expected many mined blocks");

        // check a block really was mined on genesis and the genesis node index increased
        ctx_genesis_node
            .wait_until_block_index_height(genesis_starting_index_height + 1, max_seconds)
            .await?;

        let result_genesis = block_index_endpoint_request(
            &local_test_url(&ctx_genesis_node.node_ctx.config.node_config.http.bind_port),
            0,
            genesis_starting_index_height + 1,
        )
        .await;

        let result_peer1 = poll_until_fetch_at_block_index_height(
            "peer1".to_owned(),
            &ctx_peer1_node.node_ctx,
            genesis_starting_index_height + 1,
            2000,
        )
        .await;

        //now see if the block makes its way to peer2 via gossip service
        let result_peer2 = poll_until_fetch_at_block_index_height(
            "peer2".to_owned(),
            &ctx_peer2_node.node_ctx,
            genesis_starting_index_height + 1,
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
    }

    // shut down peer nodes and then genesis node, we have what we need
    tokio::join!(
        ctx_peer1_node.stop(),
        ctx_peer2_node.stop(),
        ctx_genesis_node.stop(),
    );

    Ok(())
}

/// helper function to reduce replication of local ip in codebase
fn local_test_url(port: &u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// generate a test transaction, submit it to be added to mempool, return txn hashmap
async fn generate_test_transaction_and_add_to_block(
    node: &IrysNodeTest<IrysNodeCtx>,
    account: &IrysSigner,
) -> HashMap<IrysTransactionId, irys_types::DataTransaction> {
    let data_bytes = "Test transaction!".as_bytes().to_vec();
    let mut irys_txs: HashMap<IrysTransactionId, DataTransaction> = HashMap::new();
    match node.post_publish_data_tx(account, data_bytes).await {
        Ok(tx) => {
            irys_txs.insert(tx.header.id, tx);
        }
        Err(AddTxError::TxIngress(TxIngressError::Unfunded(tx_id))) => {
            panic!("unfunded account error for tx {}", tx_id)
        }
        Err(AddTxError::TxIngress(TxIngressError::Skipped)) => {}
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
) -> Option<reqwest::Response> {
    let mut attempts = 0;
    let max_attempts = max_attempts * 10;
    let url = local_test_url(&node_ctx.config.node_config.http.bind_port);
    loop {
        let response = info_endpoint_request(&url).await;

        if max_attempts < attempts {
            error!(
                "{} never fully synced to height {}",
                node_name, required_blocks_height
            );
            break;
        } else {
            attempts += 1;
        }

        let json_response: NodeInfo = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                debug!(
                    "{} attempt {}: failed to parse NodeInfo JSON: {}",
                    node_name, attempts, e
                );
                // wait briefly and retry
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        debug!("Fetched info endpoint response: {:?}", json_response);
        if required_blocks_height > json_response.block_index_height {
            tracing::debug!(
                "{} attempt {} checking {}. required_blocks_height > json_response.block_index_height {} > {}",
                node_name, &attempts, &url, required_blocks_height, json_response.block_index_height
            );
            //wait one second and try again
            sleep(Duration::from_millis(100)).await;
        } else {
            return Some(
                block_index_endpoint_request(
                    &local_test_url(&node_ctx.config.node_config.http.bind_port),
                    0,
                    required_blocks_height,
                )
                .await,
            );
        }
    }
    None
}
