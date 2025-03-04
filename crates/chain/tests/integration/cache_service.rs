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
use irys_actors::SolutionFoundMessage;
use irys_api_server::routes::tx::TxOffset;
use irys_chain::start_irys_node;
use irys_config::IrysNodeConfig;
use irys_database::get_chunk_cache_size;
use irys_reth_node_bridge::adapter::node::RethNodeContext;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{irys::IrysSigner, Address};
use irys_types::{Base64, Config, IrysTransactionHeader, TxChunkOffset, UnpackedChunk};

use k256::ecdsa::SigningKey;
use reth::providers::BlockReader as _;
use reth::rpc::eth::EthApiServer;
use reth_db::Database as _;
use reth_primitives::irys_primitives::precompile::IrysPrecompileOffsets;
use reth_primitives::irys_primitives::range_specifier::{
    ByteRangeSpecifier, PdAccessListArgSerde, U18, U34,
};
use reth_primitives::{irys_primitives::range_specifier::ChunkRangeSpecifier, GenesisAccount};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::utils::{capacity_chunk_solution, future_or_mine_on_timeout};

#[actix_web::test]
async fn serial_test_cache_pruning() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("serial_test_cache_pruning"), false);
    let mut testnet_config = Config::testnet();
    testnet_config.chunk_size = 32;
    testnet_config.finalization_depth = 2;
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
    ]);
    let storage_config = irys_types::StorageConfig::new(&testnet_config);

    let node = start_irys_node(config, storage_config, testnet_config.clone()).await?;
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

    let id: String = tx.header.id.as_bytes().to_base58();
    let mut tx_header_fut = Box::pin(async {
        let delay = Duration::from_secs(1);
        // sleep(delay).await;
        // println!("slept");
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
            tx_offset: TxChunkOffset::from(tx_chunk_offset as u32),
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

    let start_offset = future_or_mine_on_timeout(
        node.clone(),
        &mut start_offset_fut,
        Duration::from_millis(500),
        node.vdf_steps_guard.clone(),
        &node.vdf_config,
        &node.storage_config,
    )
    .await?
    .unwrap();

    // mine a couple blocks
    let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;
    let (chunk_cache_count, _) = &node
        .db
        .view_eyre(|tx| get_chunk_cache_size(tx, testnet_config.chunk_size))?;

    assert_eq!(*chunk_cache_count, tx.chunks.len() as u64);

    for i in 1..4 {
        info!("manually producing block {}", i);
        let poa_solution = capacity_chunk_solution(
            node.config.mining_signer.address(),
            node.vdf_steps_guard.clone(),
            &node.vdf_config,
            &node.storage_config,
        )
        .await;
        let fut = node
            .actor_addresses
            .block_producer
            .send(SolutionFoundMessage(poa_solution.clone()));
        let (block, _reth_exec_env) = fut.await??.unwrap();

        //check reth for built block
        let reth_block = reth_context
            .inner
            .provider
            .block_by_hash(block.evm_block_hash)?
            .unwrap();

        // check irys DB for built block
        let db_irys_block = &node
            .db
            .view_eyre(|tx| irys_database::block_header_by_hash(tx, &block.block_hash))?
            .unwrap();
        assert_eq!(db_irys_block.evm_block_hash, reth_block.hash_slow());
        // MAGIC: we wait more than 1s so that the block timestamps (evm block timestamps are seconds) don't overlap
        sleep(Duration::from_millis(1500)).await;
    }
    let (chunk_cache_count, _) = &node
        .db
        .view_eyre(|tx| get_chunk_cache_size(tx, testnet_config.chunk_size))?;
    assert_eq!(*chunk_cache_count, 0);

    // make sure we can read the chunks
    let chunk_res = client
        .get(format!(
            "{}/v1/chunk/ledger/0/{}",
            http_url, start_offset.data_start_offset
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(chunk_res.status(), StatusCode::OK);
    Ok(())
}
