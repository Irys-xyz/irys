use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;

use irys_database::db::IrysDatabaseExt as _;
use irys_database::{
    get_cache_size,
    tables::{CachedChunks, IngressProofs},
    walk_all,
};
use irys_types::irys::IrysSigner;
use irys_types::{Base64, DataLedger, NodeConfig, TxChunkOffset, UnpackedChunk};
use reth_db::transaction::DbTxMut as _;
use reth_db::Database as _;
use std::time::Duration;
use tracing::info;

#[test_log::test(tokio::test)]
async fn heavy3_test_cache_pruning() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().block_migration_depth = 2;
    config.cache.cache_clean_lag = 5;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 5;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 3;

    let main_address = config.miner_address();
    let account1 = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![
        (
            main_address,
            GenesisAccount {
                balance: U256::from(1000000000000000000_u128),
                ..Default::default()
            },
        ),
        (
            account1.address(),
            GenesisAccount {
                balance: U256::from(100000000000000000_u128),
                ..Default::default()
            },
        ),
    ]);
    let node = IrysNodeTest::new_genesis(config).start().await;

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
    info!("HTTP server started");

    // mine block 1 and confirm height is exactly what we need
    node.mine_block().await?;
    assert_eq!(node.get_canonical_chain_height().await, 1_u64);

    let _block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;

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
            node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();

    // post data tx
    let resp = client
        .post(format!("{}/v1/tx", http_url))
        .json(&tx.header)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

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
            .json(&chunk)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
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
            .view(|read_tx| walk_all::<IngressProofs, _>(read_tx))
            .unwrap()
            .unwrap();
        if ingress_proofs.len() == expected_proofs {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(ingress_proofs.len(), expected_proofs);

    // now chunks have been posted. mine some blocks to get the publish ledger to be updated in the latest block
    node.mine_blocks(3).await?;

    // confirm that we have one entry in CachedChunks mdbx table
    node.wait_for_chunk_cache_count(1, 10).await?;

    // post a data tx to cause the submit ledger to grow (and fill a partition)
    let chunks = vec![
        [10; 32], [20; 32], [30; 32], [40; 32], [50; 32], [60; 32], [70; 32], [80; 32], [90; 32],
    ];
    let mut data: Vec<u8> = Vec::new();
    for chunk in chunks {
        data.extend_from_slice(&chunk);
    }
    let price_info = node
        .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = account1
        .create_publish_transaction(
            data,
            node.get_anchor().await?,
            price_info.perm_fee.into(),
            price_info.term_fee.into(),
        )
        .unwrap();
    let tx = account1.sign_transaction(tx).unwrap();
    node.post_data_tx_raw(&tx.header).await;

    // mine enough blocks to cause the submit ledger to expire a partition
    node.mine_blocks(9).await?;

    // Manually remove ingress proofs to ensure there are no active proofs for a root
    node.node_ctx.db.update(|tx| {
        tx.clear::<IngressProofs>()?;
        eyre::Ok(())
    })??;

    node.mine_blocks(1).await?;

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

    assert_eq!(chunk_res.status(), reqwest::StatusCode::OK);

    node.stop().await;

    Ok(())
}
