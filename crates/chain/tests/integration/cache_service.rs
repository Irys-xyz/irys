use crate::utils::{mine_block, IrysNodeTest};
use actix_http::StatusCode;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use base58::ToBase58 as _;
use irys_actors::packing::wait_for_packing;
use irys_api_server::routes::tx::TxOffset;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{
    get_cache_size,
    tables::{CachedChunks, IngressProofs},
    walk_all,
};
use irys_types::irys::IrysSigner;
use irys_types::{Base64, NodeConfig, TxChunkOffset, UnpackedChunk};
use reth_db::Database;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};

#[test_log::test(actix_web::test)]
async fn heavy_test_cache_pruning() -> eyre::Result<()> {
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().chunk_migration_depth = 2;
    config.cache.cache_clean_lag = 50;

    let main_address = config.miner_address();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
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
    ]);
    let node = IrysNodeTest::new_genesis(config).start().await;

    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    let app = node.start_public_api().await;

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

    // mine block 1 and confirm height is exactly what we need
    node.mine_block().await?;
    assert_eq!(node.get_height().await, 1_u64);

    let block = node.get_block_by_height(node.get_height().await).await?;
    let anchor = Some(block.block_hash);

    // create and sign a data tx
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();
    let tx = account1
        .create_transaction(data_bytes.clone(), anchor)
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // post data tx
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .send_json(&tx.header)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let _ = node.mine_block().await;
    assert_eq!(node.get_height().await, 2_u64);

    // upload chunk(s)
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());

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

    // confirm that we have the right number of CachedChunks in mdbx table
    let (chunk_cache_count, _) = &node.node_ctx.db.view_eyre(|tx| {
        get_cache_size::<CachedChunks, _>(tx, node.node_ctx.config.consensus.chunk_size)
    })?;

    assert_eq!(*chunk_cache_count, tx.chunks.len() as u64);

    // confirm that we have the right number of IngressProofs in mdbx table
    let expected_proofs = 1;
    let mut ingress_proofs = vec![];
    for _ in 0..20 {
        ingress_proofs = node
            .node_ctx
            .db
            .view(walk_all::<IngressProofs, _>)
            .unwrap()
            .unwrap();
        tracing::error!(?ingress_proofs);
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if ingress_proofs.len() == expected_proofs {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(ingress_proofs.len(), expected_proofs);

    // wait for the chunks to migrate
    let id: String = tx.header.id.as_bytes().to_base58();
    let mut start_offset = None;
    let delay = Duration::from_secs(1);

    // now chunks have been posted. mine a block to get the publish ledger to be updated in the latest block
    let _ = node.mine_block().await;

    // wait for the first set of chunks to appear in the publish ledger
    let result = node.wait_for_chunk(&app, DataLedger::Publish, 0, 20).await;
    assert!(result.is_ok());

    for attempt in 1..20 {
        let mut response = client
            .get(format!(
                "{}/v1/tx/{}/local/data_start_offset",
                http_url, &id
            ))
            .send()
            .await
            .unwrap();

        match response.status() {
            StatusCode::OK => {
                let res: TxOffset = response.json().await.unwrap();
                debug!("start offset: {:?}", &res);
                info!("Transaction was retrieved ok after {} attempts", attempt);
                start_offset = Some(res);
                break;
            }
            StatusCode::NOT_FOUND => {
                sleep(delay).await;
                let _ = node.mine_block().await;
            }
            _ => {
                panic!("unexpected status type from api end point")
            }
        }
    }

    // confirm that we no longer have any CachedChunks in mdbx table
    let (chunk_cache_count, _) = &node.node_ctx.db.view_eyre(|tx| {
        get_cache_size::<CachedChunks, _>(tx, node.node_ctx.config.consensus.chunk_size)
    })?;

    assert_eq!(*chunk_cache_count, 0_u64);

    // mine enough blocks to cause chunk migration
    let _ = node.mine_block().await;

    let (chunk_cache_count, _) = &node.node_ctx.db.view_eyre(|tx| {
        get_cache_size::<CachedChunks, _>(tx, node.node_ctx.config.consensus.chunk_size)
    })?;
    assert_eq!(*chunk_cache_count, 0);

    // make sure we can read the chunks after migration
    let chunk_res = client
        .get(format!(
            "{}/v1/chunk/ledger/0/{}",
            http_url,
            start_offset.unwrap().data_start_offset
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(chunk_res.status(), StatusCode::OK);

    node.stop().await;

    Ok(())
}
