use actix_http::StatusCode;
use alloy_eips::BlockNumberOrTag;
use base58::ToBase58;
use irys_actors::packing::wait_for_packing;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::irys::IrysSigner;
use irys_types::{Config, IrysTransactionHeader};

use crate::utils::{future_or_mine_on_timeout, mine_blocks};
use reth::rpc::eth::EthApiServer;
use reth_primitives::GenesisAccount;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

#[actix_web::test]
async fn serial_should_resume_from_the_same_block() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("node_resume_test"), false);
    let mut testnet_config = Config::testnet();
    testnet_config.chunk_size = 32;

    let main_address = testnet_config.miner_address();
    let account1 = IrysSigner::random_signer(&testnet_config);
    let mut config = IrysNodeConfig {
        base_directory: temp_dir.path().to_path_buf(),
        ..IrysNodeConfig::new(&testnet_config)
    };
    config.extend_genesis_accounts(vec![
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
    let storage_config = irys_types::StorageConfig::new(&testnet_config);

    let node = start_irys_node(
        config.clone(),
        storage_config.clone(),
        testnet_config.clone(),
    )
    .await?;
    wait_for_packing(
        node.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    let http_url = "http://127.0.0.1:8080";

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
    let id: String = tx.header.id.as_bytes().to_base58();
    let mut tx_header_fut = Box::pin(async {
        let delay = Duration::from_secs(1);
        for attempt in 1..20 {
            let mut response = client
                .get(format!("{}/v1/tx/{}", http_url, &id))
                .send()
                .await
                .unwrap();

            if response.status() == StatusCode::OK {
                let result: IrysTransactionHeader = response.json().await.unwrap();
                assert_eq!(&tx.header, &result);
                info!("Transaction was retrieved ok after {} attempts", attempt);
                break;
            }
            sleep(delay).await;
        }
    });

    future_or_mine_on_timeout(
        node.clone(),
        &mut tx_header_fut,
        Duration::from_millis(500),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await?;

    mine_blocks(&node, 1).await?;
    // Waiting a little for the block
    tokio::time::sleep(Duration::from_secs(1)).await;

    let latest_block_before_restart = {
        let context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

        let latest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest.unwrap()
    };

    // Add one block on top to confirm previous one
    mine_blocks(&node, 1).await?;
    // Waiting a little for the block
    tokio::time::sleep(Duration::from_secs(1)).await;

    debug!("Stopping node");
    node.stop().await;
    // That shouldn't be necessary, but just in case
    tokio::time::sleep(Duration::from_secs(1)).await;

    debug!("Restarting node");
    let restarted_node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let (latest_block_right_after_restart, earliest_block) = {
        let context = RethNodeContext::new(restarted_node.reth_handle.clone().into()).await?;

        let latest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        let earliest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Earliest, false)
            .await?;

        (latest.unwrap(), earliest.unwrap())
    };

    mine_blocks(&restarted_node, 1).await?;

    let next_block = {
        let context = RethNodeContext::new(restarted_node.reth_handle.clone().into()).await?;

        let latest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest.unwrap()
    };

    tokio::time::sleep(Duration::from_secs(2)).await;
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
    // Check that the header hash is the same
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
