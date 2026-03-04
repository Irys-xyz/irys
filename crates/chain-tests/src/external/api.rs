use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;

use irys_api_server::routes::tx::TxOffset;
use irys_database::tables::IngressProofs;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, IrysAddress, NodeConfig};
use reth_db::transaction::DbTx as _;
use reth_db::Database as _;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

const _DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

#[ignore]
#[tokio::test]
/// This test is the counterpart test to the external API basic test in the JS Client https://github.com/Irys-xyz/irys-js
/// It waits for a valid storage tx header & chunks, mines and confirms it, then mines a couple more blocks.
/// we then halt so the client has time to read everything it needs to.
/// Instructions:
/// Run this test, until you see `waiting for tx header...`, then start the JS client test
/// that's it!, just kill this test once the JS client test finishes.
async fn external_api() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var("RUST_LOG", "debug,irys_actors::mining=error,irys_actors::packing=error,irys_chain::vdf=off,irys_vdf::vdf_state=off")
    };
    initialize_tracing();
    let mut testing_config = NodeConfig::testing();
    testing_config.http.bind_port = 8080; // external test, should never be run concurrently
    testing_config.http.bind_ip = Some("0.0.0.0".to_string());

    let account1 = IrysSigner::random_signer(&testing_config.consensus_config());
    let mut node = IrysNodeTest::new_genesis(testing_config.clone());
    let main_address = node.cfg.miner_address();
    node.cfg.consensus.extend_genesis_accounts(vec![
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
            // below is the wallet hardcoded in the JS client test
            IrysAddress::from_slice(hex::decode(DEV_ADDRESS)?.as_slice()),
            GenesisAccount {
                balance: U256::from(42000000000000000000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    let node = node.start().await;
    info!("started node {}", &node.cfg.http.bind_port);

    node.node_ctx.stop_mining()?;
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
    info!("HTTP server started {}", http_url);

    info!("waiting for tx header...");

    let (submit, _publish, _commitment) = node
        .wait_for_mempool_best_txs_shape(1, 0, 1, 999999)
        .await?;
    let recv_tx = submit.first().unwrap();

    info!(
        "got tx {:?}- waiting for chunks & ingress proof generation...",
        &recv_tx.id
    );
    let tx_id = recv_tx.id;

    // now we wait for an ingress proof to be generated for this tx (automatic once all chunks have been uploaded)
    let ingress_proof = loop {
        // don't reuse the tx! it has read isolation (won't see anything committed after it's creation)
        let ro_tx = &node.node_ctx.db.0.tx().unwrap();
        match ro_tx.get::<IngressProofs>(recv_tx.data_root).unwrap() {
            Some(ip) => break ip,
            None => sleep(Duration::from_millis(100)).await,
        }
    };

    info!(
        "got ingress proof for data root {}",
        &ingress_proof.proof.data_root
    );
    assert_eq!(&ingress_proof.proof.data_root, &recv_tx.data_root);

    let id: String = tx_id.to_string();

    // wait for the chunks to migrate
    let mut start_offset_fut = Box::pin(async {
        let delay = Duration::from_secs(1);

        for attempt in 1..20 {
            let response = client
                .get(format!(
                    "{}/v1/tx/{}/local/data-start-offset",
                    http_url, &id
                ))
                .send()
                .await
                .unwrap();

            if response.status() == reqwest::StatusCode::OK {
                let res: TxOffset = response.json().await.unwrap();
                debug!("start offset: {:?}", &res);
                info!("Transaction was retrieved ok after {} attempts", attempt);
                return Some(res);
            }
            sleep(delay).await;
        }
        None
    });

    let _start_offset = node
        .future_or_mine_on_timeout(&mut start_offset_fut, Duration::from_millis(500))
        .await?
        .unwrap();

    node.mine_blocks(10).await?;

    // sleep so the client has a chance to read the chunks
    sleep(Duration::from_millis(100_000)).await;

    Ok(())
}
