use crate::utils::{capacity_chunk_solution, future_or_mine_on_timeout};
use actix_http::StatusCode;
use alloy_core::primitives::U256;
use base58::ToBase58;
use irys_actors::mempool_service::GetBestMempoolTxs;
use irys_actors::packing::wait_for_packing;
use irys_actors::SolutionFoundMessage;
use irys_api_server::routes::tx::TxOffset;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_database::tables::IngressProofs;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, Address, Config};
use reth_db::transaction::DbTx;
use reth_db::Database as _;
use reth_primitives::GenesisAccount;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

const _DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

#[ignore]
#[actix_web::test]
/// This test is the counterpart test to the external API basic test in the JS Client https://github.com/Irys-xyz/irys-js
/// It waits for a valid storage tx header & chunks, mines and confirms it, then mines a couple more blocks.
/// we then halt so the client has time to read everything it needs to.
/// Instructions:
/// Run this test, until you see `waiting for tx header...`, then start the JS client test
/// that's it!, just kill this test once the JS client test finishes.
async fn external_api() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug,irys_actors::mining=error,irys_actors::packing=error,irys_chain::vdf=off,irys_vdf::vdf_state=off");
    let temp_dir = setup_tracing_and_temp_dir(Some("external_api"), false);

    let mut testnet_config = Config::testnet();
    testnet_config.port = 8080; // external test, should never be run concurrently

    let mut config = IrysNodeConfig::new(&testnet_config);
    config.base_directory = temp_dir.path().to_path_buf();

    let storage_config = irys_types::StorageConfig::new(&testnet_config);
    let main_address = config.mining_signer.address();
    let account1 = IrysSigner::random_signer(&testnet_config);

    config.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(690000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(420000000000000_u128),
                ..Default::default()
            },
        ),
        (
            Address::from_slice(hex::decode(DEV_ADDRESS)?.as_slice()),
            GenesisAccount {
                balance: U256::from(4200000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);

    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
    node.actor_addresses.stop_mining()?;
    wait_for_packing(
        node.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    let http_url = format!("http://127.0.0.1:{}", node.config.port);

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::default();

    let response = client
        .get(format!("{}/v1/info", http_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    info!("HTTP server started on port {}", node.config.port);

    info!("waiting for tx header...");

    let recv_tx = loop {
        let txs = node.actor_addresses.mempool.send(GetBestMempoolTxs).await;
        match txs {
            Ok(transactions) if !transactions.is_empty() => {
                break transactions[0].clone();
            }
            _ => {
                sleep(Duration::from_millis(100)).await;
            }
        }
    };
    info!(
        "got tx {:?}- waiting for chunks & ingress proof generation...",
        &recv_tx.id
    );
    let tx_id = recv_tx.id;

    // now we wait for an ingress proof to be generated for this tx (automatic once all chunks have been uploaded)
    let ingress_proof = loop {
        // don't reuse the tx! it has read isolation (won't see anything committed after it's creation)
        let ro_tx = &node.db.0.tx().unwrap();
        match ro_tx.get::<IngressProofs>(recv_tx.data_root).unwrap() {
            Some(ip) => break ip,
            None => sleep(Duration::from_millis(100)).await,
        }
    };

    info!(
        "got ingress proof for data root {}",
        &ingress_proof.data_root
    );
    assert_eq!(&ingress_proof.data_root, &recv_tx.data_root);

    let id: String = tx_id.as_bytes().to_base58();

    // wait for the chunks to migrate
    let mut start_offset_fut = Box::pin(async {
        let delay = Duration::from_secs(1);

        for attempt in 1..20 {
            let mut response = client
                .get(format!(
                    "{}/v1/tx/{}/local/data_start_offset",
                    http_url, &id
                ))
                .send()
                .await
                .unwrap();

            if response.status() == StatusCode::OK {
                let res: TxOffset = response.json().await.unwrap();
                debug!("start offset: {:?}", &res);
                info!("Transaction was retrieved ok after {} attempts", attempt);
                return Some(res);
            }
            sleep(delay).await;
        }
        None
    });

    let _start_offset = future_or_mine_on_timeout(
        node.clone(),
        &mut start_offset_fut,
        Duration::from_millis(500),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await?
    .unwrap();

    for _i in 1..10 {
        let poa_solution = capacity_chunk_solution(
            node.node_config.mining_signer.address(),
            node.vdf_steps_guard.clone(),
            &node.vdf_config,
            &node.storage_config,
        )
        .await;

        let _ = node
            .actor_addresses
            .block_producer
            .send(SolutionFoundMessage(poa_solution.clone()))
            .await?
            .unwrap();
    }

    // sleep so the client has a chance to read the chunks
    sleep(Duration::from_millis(100_000)).await;

    Ok(())
}
