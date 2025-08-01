use actix_http::StatusCode;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use base58::ToBase58 as _;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_actors::packing::wait_for_packing;
use irys_api_server::routes::tx::TxOffset;
use irys_database::tables::IngressProofs;
use irys_primitives::precompile::IrysPrecompileOffsets;
use irys_types::{irys::IrysSigner, Address, NodeConfig};
use k256::ecdsa::SigningKey;
use reth_db::transaction::DbTx as _;
use reth_db::Database as _;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::utils::{future_or_mine_on_timeout, mine_blocks, IrysNodeTest};

// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

#[ignore]
#[actix_web::test]
/// This test is the counterpart test to the programmable data basic test in the JS Client https://github.com/Irys-xyz/irys-js
/// It waits for a valid storage tx header & chunks, mines and confirms it, then mines a couple more blocks, which will include the programmable data EVM tx.
/// we then halt so the client has time to make the getStorage call and read the contract state.
/// Instructions:
/// Run this test, until you see `waiting for tx header...`, then start the JS client test
/// that's it!, just kill this test once the JS client test finishes.
async fn test_programmable_data_basic_external() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "info");

    let mut config = NodeConfig::testing();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    let main_address = config.miner_address();
    config.consensus.extend_genesis_accounts(vec![
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

    let node = IrysNodeTest::new_genesis(config.clone()).start().await;
    node.node_ctx.stop_mining().await?;
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    // let signer: PrivateKeySigner = config.mining_signer.signer.into();
    // let wallet = EthereumWallet::from(signer.clone());

    // use a constant signer so we get constant deploy addresses (for the same bytecode!)
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);

    let alloy_provider = ProviderBuilder::new().wallet(wallet).connect_http(
        format!(
            "http://127.0.0.1:{}/v1/execution-rpc",
            node.node_ctx.config.node_config.http.bind_port
        )
        .parse()?,
    );

    let deploy_builder =
        IrysProgrammableDataBasic::deploy_builder(alloy_provider.clone()).gas(29506173);

    let mut deploy_fut = Box::pin(deploy_builder.deploy());

    let contract_address = future_or_mine_on_timeout(
        node.node_ctx.clone(),
        &mut deploy_fut,
        Duration::from_millis(500),
    )
    .await??;

    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider.clone());

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();
    info!(
        "Contract address is {:?}, precompile address is {:?}",
        contract.address(),
        precompile_address
    );

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

    info!("waiting for tx header...");

    let recv_tx = loop {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let response = node
            .node_ctx
            .service_senders
            .mempool
            .send(MempoolServiceMessage::GetBestMempoolTxs(None, oneshot_tx));
        if let Err(e) = response {
            tracing::error!("channel closed, unable to send to mempool: {:?}", e);
        }
        match oneshot_rx.await {
            Ok(Ok(mempool_tx)) if !mempool_tx.submit_tx.is_empty() => {
                break mempool_tx.submit_tx[0].clone();
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
        let ro_tx = &node.node_ctx.db.0.tx().unwrap();
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
        node.node_ctx.clone(),
        &mut start_offset_fut,
        Duration::from_millis(500),
    )
    .await?
    .unwrap();

    mine_blocks(&node.node_ctx, 10).await?;

    // sleep so the client has a chance to read the chunks
    sleep(Duration::from_millis(100_000)).await;

    Ok(())
}
