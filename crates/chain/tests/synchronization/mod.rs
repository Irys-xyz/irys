use crate::utils::IrysNodeTest;
use actix_http::StatusCode;
use alloy_eips::BlockNumberOrTag;
use alloy_genesis::GenesisAccount;
use irys_actors::packing::wait_for_packing;
use irys_types::{irys::IrysSigner, NodeConfig};
use reth::rpc::eth::EthApiServer as _;
use std::time::Duration;
use tracing::{debug, info};

#[test_log::test(actix_web::test)]
async fn heavy_should_resume_from_the_same_block() -> eyre::Result<()> {
    // settings
    let max_seconds = 10;

    //setup config
    let mut config = NodeConfig::testnet();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let main_address = config.miner_address();
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(420000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    // retrieve block_migration_depth for use later
    let mut consensus = node.cfg.consensus.clone();
    let block_migration_depth: u64 = consensus.get_mut().block_migration_depth.into();

    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    let http_url = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::default();

    let response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    info!("HTTP server started");

    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    // post a tx, mine a block
    let tx = account1
        .create_transaction(data_bytes.clone(), None)
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // post tx header
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .send_json(&tx.header)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

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
    node.wait_until_height_on_chain(1_u64, max_seconds).await?;

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
