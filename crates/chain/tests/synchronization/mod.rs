use actix_http::StatusCode;
use alloy_core::primitives::aliases::U200;
use alloy_core::primitives::U256;
use alloy_eips::eip2930::AccessListItem;
use alloy_eips::BlockNumberOrTag;
use alloy_network::EthereumWallet;
use alloy_provider::ProviderBuilder;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_macro::sol;
use base58::ToBase58;
use irys_actors::packing::wait_for_packing;
use irys_api_server::routes::tx::TxOffset;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, Address};
use irys_types::{Base64, Config, IrysTransactionHeader, TxChunkOffset, UnpackedChunk};

use crate::utils::{future_or_mine_on_timeout, mine_blocks};
use k256::ecdsa::SigningKey;
use reth::rpc::eth::EthApiServer;
use reth_cli_runner::tokio_runtime;
use reth_primitives::irys_primitives::precompile::IrysPrecompileOffsets;
use reth_primitives::irys_primitives::range_specifier::{
    ByteRangeSpecifier, PdAccessListArgSerde, U18, U34,
};
use reth_primitives::{irys_primitives::range_specifier::ChunkRangeSpecifier, GenesisAccount};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysProgrammableDataBasic,
    "../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json"
);

const DEV_PRIVATE_KEY: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

#[actix_web::test]
async fn should_resume_from_the_same_block() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");

    let temp_dir = setup_tracing_and_temp_dir(Some("test_programmable_data_basic"), false);
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
        (
            Address::from_slice(hex::decode(DEV_ADDRESS)?.as_slice()),
            GenesisAccount {
                balance: alloy_core::primitives::U256::from(4200000000000000000_u128),
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

    // let signer: PrivateKeySigner = config.mining_signer.signer.into();
    // let wallet = EthereumWallet::from(signer.clone());

    // use a constant signer so we get constant deploy addresses (for the same bytecode!)
    let dev_wallet = hex::decode(DEV_PRIVATE_KEY)?;
    let signer: PrivateKeySigner = SigningKey::from_slice(dev_wallet.as_slice())?.into();
    let wallet = EthereumWallet::from(signer);

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http("http://localhost:8080/v1/execution-rpc".parse()?);

    let deploy_builder =
        IrysProgrammableDataBasic::deploy_builder(alloy_provider.clone()).gas(29506173);

    let mut deploy_fut = Box::pin(deploy_builder.deploy());

    let contract_address = future_or_mine_on_timeout(
        node.clone(),
        &mut deploy_fut,
        Duration::from_millis(500),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await??;

    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider.clone());

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();
    info!(
        "Contract address is {:?}, precompile address is {:?}",
        contract.address(),
        precompile_address
    );

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

    let latest = {
        let context = RethNodeContext::new(node.reth_handle.clone().into()).await?;

        let latest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest
    };

    node.stop().await;

    let latest_block = latest.unwrap();
    debug!("Latest block: {:?}", latest_block);

    let restarted_node = start_irys_node(config, storage_config, testnet_config.clone()).await?;

    let latest_block_right_after_restart = {
        let context = RethNodeContext::new(restarted_node.reth_handle.clone().into()).await?;

        let latest = context
            .rpc
            .inner
            .eth_api()
            .block_by_number(BlockNumberOrTag::Latest, false)
            .await?;

        latest.unwrap()
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

    debug!("Latest before stop: {:?}", latest_block.header.hash);
    debug!(
        "Latest parent before stop: {:?}",
        latest_block.header.parent_hash
    );

    debug!(
        "Block hash right after restart: {:?}",
        latest_block_right_after_restart.header.hash
    );

    debug!(
        "Hash after a block was mined after restart: {:?}",
        next_block.header.hash
    );
    debug!("Parent hash: {:?}", next_block.header.parent_hash);

    Ok(())
}
