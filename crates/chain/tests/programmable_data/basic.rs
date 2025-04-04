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
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_types::{irys::IrysSigner, Address};
use irys_types::{Base64, Config, IrysTransactionHeader, TxChunkOffset, UnpackedChunk};

use k256::ecdsa::SigningKey;
use reth::rpc::eth::EthApiServer;
use reth_primitives::irys_primitives::precompile::IrysPrecompileOffsets;
use reth_primitives::irys_primitives::range_specifier::{
    ByteRangeSpecifier, PdAccessListArgSerde, U18, U34,
};
use reth_primitives::{irys_primitives::range_specifier::ChunkRangeSpecifier, GenesisAccount};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::utils::{future_or_mine_on_timeout, IrysNodeTest};

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

#[test_log::test(actix_web::test)]
async fn heavy_test_programmable_data_basic() -> eyre::Result<()> {
    let mut irys_node = IrysNodeTest::new_genesis(Config {
        chunk_size: 32,
        chunk_migration_depth: 2,
        ..Config::testnet()
    });
    let main_address = irys_node.cfg.config.miner_address();
    let account1 = IrysSigner::random_signer(&irys_node.cfg.config);
    irys_node.cfg.irys_node_config.extend_genesis_accounts(vec![
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

    let node = irys_node.start().await;
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

    let alloy_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(
            format!(
                "http://127.0.0.1:{}/v1/execution-rpc",
                node.node_ctx.config.port
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
        node.node_ctx.vdf_steps_guard.clone(),
        &node.node_ctx.vdf_config,
        &node.node_ctx.storage_config,
    )
    .await??;

    let contract = IrysProgrammableDataBasic::new(contract_address, alloy_provider.clone());

    let precompile_address: Address = IrysPrecompileOffsets::ProgrammableData.into();
    info!(
        "Contract address is {:?}, precompile address is {:?}",
        contract.address(),
        precompile_address
    );

    let http_url = format!("http://127.0.0.1:{}", node.node_ctx.config.port);

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::default();

    // Waiting for the server to start
    tokio::time::sleep(Duration::from_secs(2)).await;

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

    let id: String = tx.header.id.as_bytes().to_base58();
    let mut tx_header_fut = Box::pin(async {
        let delay = Duration::from_secs(1);
        // sleep(delay).await;
        // println!("slept");
        for attempt in 1..20 {
            let response = client
                .get(format!("{}/v1/tx/{}", http_url, &id))
                .send()
                .await;

            let Some(mut response) = response.ok() else {
                sleep(delay).await;
                continue;
            };

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
        node.node_ctx.clone(),
        &mut tx_header_fut,
        Duration::from_millis(500),
        node.node_ctx.vdf_steps_guard.clone(),
        &node.node_ctx.vdf_config,
        &node.node_ctx.storage_config,
    )
    .await?;

    // upload chunk(s)
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };

        // Make a POST request with JSON payload

        let resp = client
            .post(format!("{}/v1/chunk", http_url))
            .send_json(&chunk)
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    // wait for the chunks to migrate
    let mut start_offset_fut = Box::pin(async {
        let delay = Duration::from_secs(1);

        for attempt in 1..20 {
            let response = client
                .get(format!(
                    "{}/v1/tx/{}/local/data_start_offset",
                    http_url, &id
                ))
                .send()
                .await;

            let Some(mut response) = response.ok() else {
                sleep(delay).await;
                continue;
            };

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
        node.node_ctx.vdf_steps_guard.clone(),
        &node.node_ctx.vdf_config,
        &node.node_ctx.storage_config,
    )
    .await?
    .unwrap();

    // let read_chunk = &node.chunk_provider.get_chunk_by_ledger_offset(
    //     irys_database::Ledger::Publish,
    //     start_offset.data_start_offset,
    // );

    // dbg!(read_chunk);

    // call with a range specifier index, position in the requested range to start from, and the number of chunks to read
    // let mut invocation_builder = contract.read_pd_chunk_into_storage();

    let mut invocation_builder = contract.readPdChunkIntoStorage();

    // set the access list (this lets the node know in advance what chunks you want to access)
    invocation_builder = invocation_builder.access_list(
        vec![AccessListItem {
            address: precompile_address,
            storage_keys: vec![
                ChunkRangeSpecifier {
                    partition_index: U200::from(0),
                    offset: 0,
                    chunk_count: 1_u16,
                }
                .encode()
                .into(),
                ByteRangeSpecifier {
                    index: 0,
                    chunk_offset: 0,
                    byte_offset: U18::from(0),
                    length: U34::from(data_bytes.len()),
                }
                .encode()
                .into(),
            ],
        }]
        .into(),
    );

    let invocation_call = invocation_builder.send().await?;
    let mut invocation_receipt_fut = Box::pin(invocation_call.get_receipt());
    let _res = future_or_mine_on_timeout(
        node.node_ctx.clone(),
        &mut invocation_receipt_fut,
        Duration::from_millis(500),
        node.node_ctx.vdf_steps_guard.clone(),
        &node.node_ctx.vdf_config,
        &node.node_ctx.storage_config,
    )
    .await??;

    let stored_bytes = contract.getStorage().call().await?._0;
    let stored_message = String::from_utf8(stored_bytes.to_vec())?;

    println!(
        "Original string: {}, stored string: {}",
        &message, &stored_message
    );

    assert_eq!(&message, &stored_message);

    let context = RethNodeContext::new(node.node_ctx.reth_handle.clone().into()).await?;

    let latest = context
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Latest, false)
        .await?;

    let safe = context
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Safe, false)
        .await?;

    let finalized = context
        .rpc
        .inner
        .eth_api()
        .block_by_number(BlockNumberOrTag::Finalized, false)
        .await?;

    dbg!(latest, safe, finalized);

    node.stop().await;
    Ok(())
}
