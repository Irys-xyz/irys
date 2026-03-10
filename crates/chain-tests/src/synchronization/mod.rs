use crate::utils::IrysNodeTest;
use alloy_eips::BlockNumberOrTag;
use alloy_genesis::GenesisAccount;

use irys_api_client::ApiClient as _;
use irys_chain::IrysNodeCtx;
use irys_types::{irys::IrysSigner, CommitmentTransaction, NodeConfig, H256};
use irys_types::{CommitmentTransactionV2, CommitmentTypeV2};
use reth::rpc::eth::EthApiServer as _;
use std::time::Duration;
use tracing::{debug, info};

#[test_log::test(tokio::test)]
async fn heavy_should_resume_from_the_same_block() -> eyre::Result<()> {
    // settings
    let max_seconds = 10;

    //setup config
    let mut config = NodeConfig::testing();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let main_address = config.miner_address();
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(1000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(100000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    // retrieve block_migration_depth for use later
    let mut consensus = node.cfg.consensus.clone();
    let block_migration_depth: u64 = consensus.get_mut().block_migration_depth.into();

    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // server should be running
    // check with request to `/v1/info`
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    info!("HTTP server started");

    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    // post a tx, mine a block
    // Get price from the API
    let price_info = node
        .get_data_price(irys_types::DataLedger::Publish, data_bytes.len() as u64)
        .await
        .expect("Failed to get price");

    println!("Price info: {:?}", price_info);

    let tx = account1
        .create_publish_transaction(
            data_bytes.clone(),
            node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // post tx header
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .json(&tx.header)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Check that tx has been sent
    node.wait_for_mempool(tx.header.id, max_seconds).await?;

    // mine first block on node
    node.mine_block().await?;
    node.wait_until_height(1, max_seconds).await?;

    let latest_block_before_restart = {
        let latest = node
            .node_ctx
            .reth_node_adapter
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest.unwrap()
    };

    // Add enough blocks on top to move block 1 to index
    node.mine_blocks(block_migration_depth.try_into()?).await?;
    node.wait_until_height(block_migration_depth + 1_u64, max_seconds)
        .await?;
    node.wait_until_block_index_height(1_u64, max_seconds)
        .await?;

    // restarting the node means we lose blocks in mempool
    info!("Restarting node");
    let restarted_node = node.stop().await.start().await;

    restarted_node.wait_until_height(1_u64, max_seconds).await?;

    info!("getting reth node context");
    let (latest_block_right_after_restart, earliest_block) = {
        let latest = restarted_node
            .node_ctx
            .reth_node_adapter
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        let earliest = restarted_node
            .node_ctx
            .reth_node_adapter
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Earliest, false)
            .await?;

        (latest.unwrap(), earliest.unwrap())
    };

    // mine a fresh block after the restart
    info!("mining blocks");
    restarted_node.mine_block().await?;

    let next_block = {
        let latest = restarted_node
            .node_ctx
            .reth_node_adapter
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest.unwrap()
    };

    restarted_node.wait_until_height(1, max_seconds).await?;
    restarted_node.stop().await;

    debug!("Earliest hash: {:?}", earliest_block.header.hash);
    debug!(
        "Latest parent hash: {:?}",
        latest_block_right_after_restart.header.parent_hash
    );
    debug!(
        "Latest hash before restart: {:?}",
        latest_block_before_restart.header.hash
    );
    debug!(
        "Latest hash after restart: {:?}",
        latest_block_right_after_restart.header.hash
    );
    debug!("Next block parent: {:?}", next_block.header.parent_hash);
    debug!("Next block hash: {:?}", next_block.header.hash);

    // Check that we aren't on genesis
    assert_eq!(
        earliest_block.header.hash,
        latest_block_before_restart.header.parent_hash
    );
    // Check that the header number & hash is the same
    assert_eq!(
        latest_block_before_restart.header.number,
        latest_block_right_after_restart.header.number
    );
    assert_eq!(
        latest_block_before_restart.header.hash,
        latest_block_right_after_restart.header.hash
    );
    // Check that the chain advanced correctly
    assert_eq!(
        next_block.header.parent_hash,
        latest_block_right_after_restart.header.hash
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy3_should_reject_commitment_transactions_from_unknown_sources() -> eyre::Result<()> {
    // settings
    let max_seconds = 10;

    //setup config
    let mut config = NodeConfig::testing();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let account2 = IrysSigner::random_signer(&config.consensus_config());
    let account3 = IrysSigner::random_signer(&config.consensus_config());
    let main_address = config.miner_address();
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(100000000000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(100000000000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account2.address(),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(100000000000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account3.address(),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(100000000000000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    // Originally we should only allow account1 to stake/pledge
    config.initial_stake_and_pledge_whitelist = vec![account1.address()];
    let genesis_node = IrysNodeTest::new_genesis(config.clone()).start().await;
    let mut testing_peer_config = genesis_node.testing_peer();
    // Check that even if the peer has an empty whitelist at the start, it still gets the correct
    //  whitelist from the genesis node
    testing_peer_config.initial_stake_and_pledge_whitelist = vec![];
    let peer = IrysNodeTest::new(testing_peer_config).start().await;

    // retrieve block_migration_depth for use later
    let mut consensus = genesis_node.cfg.consensus.clone();
    let block_migration_depth: u64 = consensus.get_mut().block_migration_depth.into();

    genesis_node
        .node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // Wait a little for a peer to connect
    tokio::time::sleep(Duration::from_secs(1)).await;
    let genesis_peers = genesis_node.node_ctx.peer_list.all_known_peers();
    assert_eq!(genesis_peers.len(), 1);
    let peer_socket_address = genesis_peers[0].api;

    let api_client = irys_api_client::IrysApiClient::new();

    // mine first block on node
    info!("Commitment whitelist test: mining first block");
    let first_block = genesis_node.mine_block().await?;
    info!(
        "Commitment whitelist test: waiting for a block {} to appear on peer",
        first_block.height
    );
    peer.wait_for_block_at_height(first_block.height, max_seconds)
        .await?;

    info!("Commitment whitelist test: getting block 1 from peer");
    // Check that the peer has the block mined on the genesis node
    let block_1 = api_client
        .get_block_by_height(peer_socket_address, first_block.height, true)
        .await?
        .expect("block to be accessible");

    let stake_tx = create_stake_tx(&peer, &account1, block_1.irys.block_hash).await;
    api_client
        .post_commitment_transaction(peer_socket_address, stake_tx.clone())
        .await
        .expect("post commitment tx should succeed");
    debug!(
        "Commitment whitelist test: Response from posting stake tx: {:?}",
        ()
    );
    genesis_node
        .wait_for_mempool_commitment_txs(vec![stake_tx.id()], max_seconds)
        .await?;
    info!("Commitment whitelist test: stake tx from a whitelisted account accepted");

    let mut stake_tx2 =
        CommitmentTransaction::new_stake(&config.consensus_config(), block_1.irys.block_hash);
    account2.sign_commitment(&mut stake_tx2)?;
    let response2 = api_client
        .post_commitment_transaction(peer_socket_address, stake_tx2.clone())
        .await;
    assert!(
        response2.is_err(),
        "Posting a stake tx from a non-whitelisted account should fail"
    );
    let err_string = response2.err().unwrap().to_string();
    if err_string.contains("Signer address is not allowed to stake/pledge") {
        debug!("Received expected ForbiddenSigner error");
    } else {
        panic!(
            "Expected 'Signer address is not allowed to stake/pledge' error, got: {}",
            err_string
        );
    }

    let blocks_to_be_mined = block_migration_depth + 2;
    // Add enough blocks on top to move block 1 to index
    genesis_node
        .mine_blocks(blocks_to_be_mined.try_into()?)
        .await?;
    let mut block_to_wait_for = first_block.height + blocks_to_be_mined;
    info!(
        "Commitment whitelist test: waiting for block {} to appear on genesis",
        block_to_wait_for
    );
    genesis_node
        .wait_for_block_at_height(block_to_wait_for, max_seconds)
        .await?;
    info!(
        "Commitment whitelist test: waiting for block {} to appear on peer",
        block_to_wait_for
    );
    peer.wait_for_block_at_height(block_to_wait_for, max_seconds)
        .await?;

    let genesis_peers = genesis_node.node_ctx.peer_list.all_known_peers();
    assert_eq!(genesis_peers.len(), 1);
    // Since the peer was unstaked, it's going to be removed from the peer list when the node restarts
    // so we save the address here to use later
    let genesis_peer_address = genesis_peers[0];
    // restarting the node means we lose blocks in mempool
    info!("Commitment whitelist test: Restarting node");
    let mut stopped_genesis = genesis_node.stop().await;

    stopped_genesis.cfg.initial_stake_and_pledge_whitelist = vec![
        account1.address(),
        account2.address(), // Now we add account2 to the whitelist
    ];
    stopped_genesis.cfg.trusted_peers = vec![genesis_peer_address];

    info!("Commitment whitelist test: Restarting genesis node with a new whitelist");
    let genesis_node = stopped_genesis.start().await;
    info!("Commitment whitelist test: Restarted genesis");

    info!(
        "Commitment whitelist test: waiting for block {} to appear on genesis",
        block_to_wait_for
    );
    genesis_node
        .wait_for_block_at_height(block_to_wait_for, max_seconds)
        .await?;

    // mine a couple blocks
    info!("Commitment whitelist test: mining blocks to propagate a new whitelist");
    genesis_node.mine_blocks(2).await?;
    block_to_wait_for += 2;

    info!(
        "Commitment whitelist test: waiting for block {} to appear on peer",
        block_to_wait_for
    );
    peer.wait_for_block_at_height(block_to_wait_for, max_seconds)
        .await?;

    let another_anchor_block = api_client
        .get_block_by_height(peer_socket_address, block_to_wait_for, true)
        .await?
        .expect("block to be accessible");

    let mut new_stake_tx2 = CommitmentTransaction::new_stake(
        &config.consensus_config(),
        another_anchor_block.irys.block_hash,
    );
    account2.sign_commitment(&mut new_stake_tx2)?;
    let new_response2 = api_client
        .post_commitment_transaction(peer_socket_address, new_stake_tx2.clone())
        .await;
    assert!(new_response2.is_ok());
    debug!(
        "Response from posting stake tx from a newly whitelisted account: {:?}",
        new_response2
    );
    peer.wait_for_mempool_commitment_txs(vec![new_stake_tx2.id()], max_seconds)
        .await?;

    let mut stake_tx3 = CommitmentTransaction::new_stake(
        &config.consensus_config(),
        another_anchor_block.irys.block_hash,
    );
    account3.sign_commitment(&mut stake_tx3)?;
    let response3 = api_client
        .post_commitment_transaction(peer_socket_address, stake_tx3.clone())
        .await;
    debug!(
        "Response from posting stake tx from a non-whitelisted account: {:?}",
        response3
    );
    assert!(
        response3.is_err(),
        "Posting a stake tx from a non-whitelisted account should fail"
    );
    let err_string = response3.err().unwrap().to_string();
    if err_string.contains("Signer address is not allowed to stake/pledge") {
        debug!("Received expected ForbiddenSigner error");
    } else {
        panic!(
            "Expected 'Signer address is not allowed to stake/pledge' error, got: {}",
            err_string
        );
    }

    tokio::join!(genesis_node.stop(), peer.stop(),);

    Ok(())
}

async fn create_stake_tx(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    anchor: H256,
) -> CommitmentTransaction {
    // Get stake price from API
    let price_info = node
        .get_stake_price()
        .await
        .expect("Failed to get stake price from API");

    let consensus = &node.node_ctx.config.consensus;
    let mut stake_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            commitment_type: CommitmentTypeV2::Stake,
            anchor,
            fee: price_info.fee.try_into().expect("fee should fit in u64"),
            value: price_info.value,
            ..CommitmentTransactionV2::new(consensus)
        },
        metadata: Default::default(),
    });

    info!("Created stake_tx with value: {:?}", stake_tx.value());
    signer.sign_commitment(&mut stake_tx).unwrap();
    stake_tx
}
