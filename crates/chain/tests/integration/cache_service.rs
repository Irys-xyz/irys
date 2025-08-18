use crate::utils::IrysNodeTest;
use actix_http::StatusCode;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_actors::packing::wait_for_packing;
use irys_types::{irys::IrysSigner, Base64, DataLedger, NodeConfig, TxChunkOffset, UnpackedChunk};
use std::time::Duration;
use tracing::info;

#[test_log::test(actix_web::test)]
async fn heavy_test_cache_pruning() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().block_migration_depth = 2;
    config.cache.cache_clean_lag = 5;

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
    assert_eq!(node.get_canonical_chain_height().await, 1_u64);

    let block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let anchor = Some(block.block_hash);

    // create and sign a data tx
    let message = "Hirys, world!";
    let data_bytes = message.as_bytes().to_vec();

    // Get price from the API
    let price_info = node
        .get_data_price(irys_types::DataLedger::Publish, data_bytes.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = account1
        .create_publish_transaction(
            data_bytes.clone(),
            anchor,
            price_info.perm_fee,
            price_info.term_fee,
        )
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // post data tx
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .send_json(&tx.header)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    node.mine_block().await?;
    assert_eq!(node.get_canonical_chain_height().await, 2_u64);

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

    // Wait for ingress proofs to be generated before proceeding
    node.wait_for_ingress_proofs(vec![tx.header.id], 10).await?;

    // Wait until the chunks have been migrated/persisted before asserting cache count
    node.wait_for_chunk_cache_count(tx.chunks.len() as u64, 10)
        .await?;

    // now chunks have been posted. mine some blocks to get the publish ledger to be updated in the latest block
    node.mine_blocks(3).await?;

    // confirm that we have one entry in CachedChunks mdbx table (proof pipeline processed)
    node.wait_for_chunk_cache_count(1, 10).await?;

    // mine enough blocks to cause block and chunk migration
    node.mine_blocks(node.node_ctx.config.node_config.cache.cache_clean_lag as usize)
        .await?;

    // confirm that we no longer see an entry in CachedChunks mdbx table
    node.wait_for_chunk_cache_count(0, 10).await?;

    // make sure we can read the chunks after migration
    let chunk_res = client
        .get(format!(
            "{}/v1/chunk/ledger/{}/{}",
            http_url,
            DataLedger::Publish as usize,
            0_u64,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(chunk_res.status(), StatusCode::OK);

    node.stop().await;

    Ok(())
}
