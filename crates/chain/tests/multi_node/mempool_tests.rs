use crate::utils::*;
use alloy_core::primitives::{Bytes, TxKind, B256, U256};
use alloy_eips::{BlockId, Encodable2718 as _};
use alloy_genesis::GenesisAccount;
use alloy_signer_local::LocalSigner;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_chain::IrysNodeCtx;
use irys_database::tables::IngressProofs;
use irys_reth_node_bridge::{
    ext::IrysRethRpcTestContextExt as _, reth_e2e_test_utils::transaction::TransactionTestContext,
    IrysRethNodeAdapter,
};
use irys_testing_utils::initialize_tracing;
use irys_types::CommitmentTypeV1;
use irys_types::SendTraced as _;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, ConsensusConfig, DataLedger, DataTransaction,
    IngressProofsList, IrysBlockHeader, NodeConfig, SystemLedger, H256,
};
use k256::ecdsa::SigningKey;
use rand::Rng as _;
use reth::rpc::{
    api::EthApiClient,
    types::{Block, Header, TransactionRequest},
};
use reth_db::transaction::DbTx as _;
use reth_db::Database as _;
use reth_ethereum_primitives::{Receipt, Transaction};
use std::{sync::Arc, time::Duration};
use tokio::{sync::oneshot, time::sleep};
use tracing::{debug, info};

#[tokio::test]
async fn heavy_pending_chunks_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    // retrieve block_migration_depth for use later
    let mut consensus = genesis_node.cfg.consensus.clone();
    let block_migration_depth = consensus.get_mut().block_migration_depth;

    // chunks
    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in chunks.iter() {
        data.extend_from_slice(chunk);
    }

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(irys_types::DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = signer.create_publish_transaction(
        data,
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;

    // First post the chunks
    post_chunk(&app, &tx, 0, &chunks).await;
    post_chunk(&app, &tx, 1, &chunks).await;
    post_chunk(&app, &tx, 2, &chunks).await;

    // Then post the tx (deliberately after the chunks)
    post_data_tx(&app, &tx).await;

    // wait for chunks to be in CachedChunks table
    genesis_node.wait_for_chunk_cache_count(3, 10).await?;

    // Mine some blocks to trigger block and chunk migration
    genesis_node
        .mine_blocks((1 + block_migration_depth).try_into()?)
        .await?;
    genesis_node.wait_until_block_index_height(1, 5).await?;

    // Finally verify the chunks didn't get dropped
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 0, 5)
        .await?;
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 1, 5)
        .await?;
    genesis_node
        .wait_for_chunk(&app, DataLedger::Submit, 2, 5)
        .await?;

    // teardown
    genesis_node.stop().await;

    Ok(())
}

#[tokio::test]
async fn preheader_rejects_oversized_data_path() -> eyre::Result<()> {
    use actix_web::{http::StatusCode, test};
    use irys_types::{Base64, TxChunkOffset, UnpackedChunk};

    // Turn on tracing even before the nodes start
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testing();

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Prepare a small single-chunk data payload
    let chunk_size = genesis_config.consensus_config().chunk_size as usize;
    let data = vec![7_u8; chunk_size];

    // Start the node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = signer.create_publish_transaction(
        data.clone(),
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;

    // Build a pre-header chunk with an oversized data_path (> 64 KiB)
    let oversized_path = vec![0_u8; 70_000];
    let chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(oversized_path),
        bytes: Base64(data),
        tx_offset: TxChunkOffset::from(0_u32),
    };

    // Post the chunk before the tx header
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Ensure it did not get cached
    genesis_node.wait_for_chunk_cache_count(0, 3).await?;

    // Post the tx header and confirm cache still empty
    post_data_tx(&app, &tx).await;
    genesis_node.wait_for_chunk_cache_count(0, 3).await?;

    genesis_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn preheader_rejects_oversized_bytes() -> eyre::Result<()> {
    use actix_web::{http::StatusCode, test};
    use irys_types::{Base64, TxChunkOffset, UnpackedChunk};

    initialize_tracing();

    let mut genesis_config = NodeConfig::testing();

    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Prepare bytes that exceed chunk_size by 1
    let chunk_size = genesis_config.consensus_config().chunk_size as usize;
    let data = vec![9_u8; chunk_size + 1];

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    // Get price from the API
    let tx_data = vec![1_u8; chunk_size];
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, tx_data.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = signer.create_publish_transaction(
        tx_data,
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;

    // Build a pre-header chunk with oversized bytes
    let chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(vec![]),
        bytes: Base64(data),
        tx_offset: TxChunkOffset::from(0_u32),
    };

    // Post the chunk before the tx header
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&chunk)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Ensure it did not get cached
    genesis_node.wait_for_chunk_cache_count(0, 3).await?;

    // Post the tx header and confirm cache still empty
    post_data_tx(&app, &tx).await;
    genesis_node.wait_for_chunk_cache_count(0, 3).await?;

    genesis_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn preheader_rejects_when_cache_full() -> eyre::Result<()> {
    use actix_web::{http::StatusCode, test};
    use irys_types::{Base64, TxChunkOffset, UnpackedChunk};

    initialize_tracing();

    let mut genesis_config = NodeConfig::testing();
    // Pre-header cap is min(max_chunks_per_item, 64) => default 64
    let preheader_cap: u32 = 64;

    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Prepare data large enough to have 65+ chunks (so we can fill cache with 64)
    let chunk_size = genesis_config.consensus_config().chunk_size as usize;
    let tx_data = vec![5_u8; chunk_size * (preheader_cap as usize + 1)];
    let chunk_bytes = vec![5_u8; chunk_size];

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    // Get price from the API
    let price_info = genesis_node
        .get_data_price(DataLedger::Publish, tx_data.len() as u64)
        .await
        .expect("Failed to get price");

    let tx = signer.create_publish_transaction(
        tx_data,
        genesis_node.get_anchor().await?,
        price_info.perm_fee.into(),
        price_info.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;

    // Fill the pre-header cache to capacity (64 chunks)
    for offset in 0..preheader_cap {
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(vec![]),
            bytes: Base64(chunk_bytes.clone()),
            tx_offset: TxChunkOffset::from(offset),
        };

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/v1/chunk")
                .set_json(&chunk)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Now try to add one more chunk - should be rejected (cache full)
    let overflow_chunk = UnpackedChunk {
        data_root: tx.header.data_root,
        data_size: tx.header.data_size,
        data_path: Base64(vec![]),
        bytes: Base64(chunk_bytes),
        tx_offset: TxChunkOffset::from(preheader_cap),
    };

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&overflow_chunk)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("PreHeaderOffsetExceedsCap"),
        "Expected chunk to be rejected with PreHeaderOffsetExceedsCap, got: {body_str}"
    );

    genesis_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_pending_pledges_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create stake and pledge commitments for the signer
    let config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;
    let stake_tx = new_stake_tx(&anchor, &signer, config);
    let pledge_tx = new_pledge_tx(
        &anchor,
        &signer,
        config,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
    )
    .await;

    // Post the pledge before the stake
    genesis_node.post_commitment_tx(&pledge_tx).await?;
    genesis_node.post_commitment_tx(&stake_tx).await?;

    // Wait for both transactions to be processed into the mempool
    genesis_node
        .wait_for_mempool_commitment_txs(vec![stake_tx.id(), pledge_tx.id()], 10)
        .await?;

    // Mine a block to confirm the commitments
    genesis_node.mine_block().await.unwrap();

    // Validate the SystemLedger in the block that it contains the correct commitments
    let block = genesis_node.get_block_by_height(1).await.unwrap();
    assert_eq!(
        block.system_ledgers[0].tx_ids,
        vec![stake_tx.id(), pledge_tx.id()]
    );

    genesis_node.stop().await;

    Ok(())
}

#[tokio::test]
/// Test mempool persists to disk during shutdown
///
/// FIXME: This test will not be effective until mempool tree/index separation work is complete
///
/// post stake, post pledge, restart node
/// confirm pledge is present in mempool
/// post storage tx, restart node
/// confirm storage tx is present in mempool
async fn heavy_mempool_persistence_test() -> eyre::Result<()> {
    // Turn on tracing even before the node starts
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testing();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // Create and post stake commitment for the signer
    let config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;
    let stake_tx = new_stake_tx(&anchor, &signer, config);
    genesis_node.post_commitment_tx(&stake_tx).await?;
    genesis_node.mine_block().await.unwrap();

    let expected_txs = vec![stake_tx.id()];
    let result = genesis_node
        .wait_for_mempool_commitment_txs(expected_txs, 20)
        .await;
    assert!(result.is_ok());

    //create and post pledge commitment for the signer
    let pledge_tx = new_pledge_tx(
        &anchor,
        &signer,
        config,
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
    )
    .await;
    genesis_node.post_commitment_tx(&pledge_tx).await?;

    // test storage data
    let chunks = [[10; 32], [20; 32], [30; 32]];
    let data: Vec<u8> = chunks.concat();

    // post storage tx
    let storage_tx = genesis_node
        .post_data_tx_without_gossip(anchor, data, &signer)
        .await;

    // Restart the node
    tracing::info!("Restarting node");
    let restarted_node = genesis_node.stop().await.start().await;

    // confirm the mempool data tx have appeared back in the mempool after a restart
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    let get_tx_msg = MempoolServiceMessage::GetDataTxs(vec![storage_tx.header.id], oneshot_tx);
    if let Err(err) = restarted_node
        .node_ctx
        .service_senders
        .mempool
        .send_traced(get_tx_msg)
    {
        tracing::error!("error sending message to mempool: {:?}", err);
    }
    let data_tx_from_mempool = oneshot_rx.await.expect("expected result");
    assert!(data_tx_from_mempool
        .first()
        .expect("expected a data tx")
        .is_some());

    // confirm the commitment tx has appeared back in the mempool after a restart
    let result = restarted_node
        .wait_for_mempool_commitment_txs(vec![pledge_tx.id()], 10)
        .await;
    assert!(result.is_ok());

    restarted_node.stop().await;

    Ok(())
}

#[tokio::test]
async fn heavy4_mempool_submit_tx_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=off,storage::db::mdbx=off,reth=off,irys_p2p::server=off,irys_actors::mining=error",
    );
    }

    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch: u64 = 3;
    let seconds_to_wait = 15;
    // setup config / testnet
    let block_migration_depth = num_blocks_in_epoch - 1;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;
    // TODO: change anchor
    genesis_config
        .consensus
        .get_mut()
        .mempool
        .tx_anchor_expiry_depth = 100; // don't care about anchor expiry
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;

    // Create a signer (keypair) for the peer and fund it
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();

    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis_node.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testing_peer_with_signer(&peer2_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;

    // Post stake + pledge commitments to peer1
    let peer1_stake_tx = peer1_node.post_stake_commitment(None).await?; // zero() is the genesis block hash
    let peer1_pledge_tx = peer1_node.post_pledge_commitment(None).await?;

    // Post stake + pledge commitments to peer2
    let peer2_stake_tx = peer2_node.post_stake_commitment(None).await?;
    let peer2_pledge_tx = peer2_node.post_pledge_commitment(None).await?;

    // Wait for all commitment tx to show up in the genesis_node's mempool
    genesis_node
        .wait_for_mempool(peer1_stake_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer1_pledge_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_stake_tx.id(), seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(peer2_pledge_tx.id(), seconds_to_wait)
        .await?;

    let mut expected_height = num_blocks_in_epoch;

    // Mine blocks to get the commitments included, epoch tasks performed, and assignments of partition_hash's to the peers
    genesis_node.mine_blocks(expected_height as usize).await?;

    // wait for block mining to reach tree height
    genesis_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;

    // wait for migration to reach index height
    genesis_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Get the genesis nodes view of the peers assignments
    let peer1_assignments = genesis_node.get_partition_assignments(peer1_signer.address());
    let peer2_assignments = genesis_node.get_partition_assignments(peer2_signer.address());

    // Verify that one partition has been assigned to each peer to match its pledge
    assert_eq!(peer1_assignments.len(), 1);
    assert_eq!(peer2_assignments.len(), 1);

    // Wait for the peers to receive & process the epoch block
    peer1_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Wait for them to pack their storage modules with the partition_hashes
    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    let mut rng = rand::thread_rng();
    let chunks1: [[u8; 32]; 3] = [[rng.r#gen(); 32], [rng.r#gen(); 32], [rng.r#gen(); 32]];
    let data1: Vec<u8> = chunks1.concat();

    let chunks2 = [[rng.r#gen(); 32], [rng.r#gen(); 32], [rng.r#gen(); 32]];
    let data2: Vec<u8> = chunks2.concat();

    let chunks3 = [[rng.r#gen(); 32], [rng.r#gen(); 32], [rng.r#gen(); 32]];
    let data3: Vec<u8> = chunks3.concat();

    // Post a transaction that should be gossiped to all peers
    let shared_tx = genesis_node
        .post_data_tx(
            genesis_node.get_anchor().await?,
            data3,
            &genesis_node.node_ctx.config.irys_signer(),
        )
        .await;

    // Wait for the transaction to gossip

    let txid = shared_tx.header.id;

    peer1_node.wait_for_mempool(txid, seconds_to_wait).await?;
    peer2_node.wait_for_mempool(txid, seconds_to_wait).await?;

    // Post a unique storage transaction to each peer
    let peer1_tx = peer1_node
        .post_data_tx_without_gossip(peer1_node.get_anchor().await?, data1, &peer1_signer)
        .await;
    let peer2_tx = peer2_node
        .post_data_tx_without_gossip(peer2_node.get_anchor().await?, data2, &peer2_signer)
        .await;

    // Mine mine blocks on both peers in parallel
    let (result1, result2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1)
    );

    // Fail the test on any error results
    result1?;
    result2?;

    expected_height += 1;

    // wait for block mining to reach tree height
    peer1_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;
    // wait for migration to reach index height
    peer1_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(expected_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Validate the peer blocks create forks with different transactions
    let peer1_block: Arc<IrysBlockHeader> = peer1_node
        .get_block_by_height(expected_height)
        .await?
        .into();
    let peer2_block: Arc<IrysBlockHeader> = peer2_node
        .get_block_by_height(expected_height)
        .await?
        .into();

    let peer1_block_txids = &peer1_block.data_ledgers[DataLedger::Submit].tx_ids.0;
    assert!(
        peer1_block_txids.contains(&txid),
        "block {} {} should include submit tx {}",
        &peer1_block.block_hash,
        &peer1_block.height,
        &txid
    );
    assert!(peer1_block_txids.contains(&peer1_tx.header.id));

    let peer2_block_txids = &peer2_block.data_ledgers[DataLedger::Submit].tx_ids.0;
    assert!(peer2_block_txids.contains(&txid));
    assert!(peer2_block_txids.contains(&peer2_tx.header.id));

    // Assert both blocks have the same cumulative difficulty this will ensure
    // that the peers prefer the first block they saw with this cumulative difficulty,
    // their own.
    assert_eq!(peer1_block.cumulative_diff, peer2_block.cumulative_diff);

    peer2_node.gossip_block_to_peers(&peer2_block)?;
    peer1_node.gossip_block_to_peers(&peer1_block)?;

    // Wait for gossip, to send blocks to opposite peers
    peer1_node
        .wait_for_block(&peer2_block.block_hash, 10)
        .await?;
    peer2_node
        .wait_for_block(&peer1_block.block_hash, 10)
        .await?;
    peer1_node.get_block_by_hash(&peer2_block.block_hash)?;
    peer2_node.get_block_by_hash(&peer1_block.block_hash)?;

    let peer1_block_after = peer1_node.get_block_by_height(expected_height).await?;
    let peer2_block_after = peer2_node.get_block_by_height(expected_height).await?;

    // Verify neither peer changed their blocks after receiving the other peers block
    // for the same height.
    assert_eq!(peer1_block_after.block_hash, peer1_block.block_hash);
    assert_eq!(peer2_block_after.block_hash, peer2_block.block_hash);
    debug!(
        "\nPEER1\n    before: {} c_diff: {}\n    after:  {} c_diff: {}\nPEER2\n    before: {} c_diff: {}\n    after:  {} c_diff: {}",
        peer1_block.block_hash,
        peer1_block.cumulative_diff,
        peer1_block_after.block_hash,
        peer1_block_after.cumulative_diff,
        peer2_block.block_hash,
        peer2_block.cumulative_diff,
        peer2_block_after.block_hash,
        peer2_block_after.cumulative_diff,
    );

    let _block_hash = genesis_node
        .wait_until_height(expected_height, seconds_to_wait)
        .await?;
    let genesis_block = genesis_node.get_block_by_height(expected_height).await?;

    debug!(
        "\nGENESIS: {:?} height: {}",
        genesis_block.block_hash, genesis_block.height
    );

    // Make sure the reorg_tx is back in the mempool ready to be included in the next block
    // NOTE: It turns out the reorg_tx is actually in the block because all tx are gossiped
    //       along with their blocks even if they are a fork, so when the peer
    //       extends their fork, they have the fork tx in their mempool already
    //       and it gets included in the block.
    // let pending_tx = genesis_node.get_best_mempool_tx(None).await;
    // let tx = pending_tx
    //     .submit_tx
    //     .iter()
    //     .find(|tx| tx.id == reorg_tx.header.id);
    // assert_eq!(tx, Some(&reorg_tx.header));

    // with that ^ in mind, validate that the reorg tip block has the fork submit tx included

    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);

    let canon_before = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain();

    // Determine which peer lost the fork race and extend the other peer's chain
    // to trigger a reorganization. The losing peer's transaction will be evicted
    // and returned to the mempool.
    let reorg_tx: DataTransaction;
    let _reorg_block_hash: H256;
    let reorg_block = if genesis_block.block_hash == peer1_block.block_hash {
        debug!(
            "GENESIS: should ignore {} and should already be on {} height: {}",
            peer2_block.block_hash, peer1_block.block_hash, genesis_block.height
        );
        reorg_tx = peer1_tx; // Peer1 won initially, so peer2's chain will overtake it
        peer2_node.mine_block().await?;
        expected_height += 1;
        peer2_node.get_block_by_height(expected_height).await?
    } else {
        debug!(
            "GENESIS: should ignore {} and should already be on {} height: {}",
            peer1_block.block_hash, peer2_block.block_hash, genesis_block.height
        );
        reorg_tx = peer2_tx; // Peer2 won initially, so peer1's chain will overtake it
        peer1_node.mine_block().await?;
        expected_height += 1;
        peer1_node.get_block_by_height(expected_height).await?
    };

    let reorg_event = reorg_future.await?;
    let _genesis_block = genesis_node.get_block_by_height(expected_height).await?;

    debug!("{:?}", reorg_event);
    let canon = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_canonical_chain();

    let old_fork_hashes: Vec<_> = reorg_event
        .old_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();
    let new_fork_hashes: Vec<_> = reorg_event
        .new_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();

    debug!(
        "ReorgEvent:\n fork_parent: {:?}\n old_fork: {:?}\n new_fork:{:?}",
        reorg_event.fork_parent.block_hash, old_fork_hashes, new_fork_hashes
    );

    debug!("reorg_tx: {:?}", reorg_tx.header.id);
    debug!("canonical_before: {:?}", &canon_before.0);

    debug!("canonical_after: {:?}", &canon.0);

    // Validate the ReorgEvent with the canonical chains
    let old_fork: Vec<_> = reorg_event
        .old_fork
        .iter()
        .map(|bh| bh.header().block_hash)
        .collect();

    let new_fork: Vec<_> = reorg_event
        .new_fork
        .iter()
        .map(|bh| bh.header().block_hash)
        .collect();

    debug!("fork_parent: {:?}", reorg_event.fork_parent.block_hash);
    debug!("old_fork:  {:?}", old_fork);
    debug!("new_fork:  {:?}", new_fork);

    assert_eq!(old_fork, vec![canon_before.0.last().unwrap().block_hash()]);
    assert_eq!(
        new_fork,
        vec![
            canon.0[canon.0.len() - 2].block_hash(),
            canon.0.last().unwrap().block_hash()
        ]
    );

    assert_eq!(reorg_event.new_tip, *new_fork.last().unwrap());

    assert!(reorg_block.data_ledgers[DataLedger::Submit]
        .tx_ids
        .contains(&reorg_tx.header.id));

    // Wind down test
    tokio::join!(genesis_node.stop(), peer1_node.stop(), peer2_node.stop());
    Ok(())
}

/// this test tests reorging in the context of confirmed publish ledger transactions
/// goals:
/// - ensure orphaned publish ledger txs are included in the mempool & in blocks post-reorg
/// - ensure reorged publish txs are always associated with the canonical ingress proof(s)
/// Steps:
/// create 3 nodes: A (genesis), B and C
/// mine commitments for B and C using A
/// make sure all the nodes are synchronised
/// prevent all gossip/P2P
/// send A a storage Tx, ready for promotion
/// send B a storage Tx
/// prime a fork - mine one block on A and two on B
///  the second B block should promote B's storage tx
/// assert A and B's blocks include their respective promoted tx
/// trigger a reorg - send B's txs & blocks to A
/// assert that A has a reorg event
/// assert that A's tx is returned to the mempool
/// send A's tx to B & prepare it for promotion
/// mine a block on B, assert A's tx is included correctly
/// send B's block back to A, assert mempool state ingress proofs etc are correct
// TODO: once longer forks are stable & if it's worthwhile:
/// mine 4 blocks on C
/// send  C's blocks to A
/// assert all txs return to mempool
/// send C's blocks to B
/// assert txs return to mempool
/// send returned txs to C
/// mine a block on C, assert that all reorgd txs are present
#[rstest::rstest]
#[case::full_validation(true)]
#[case::default(false)]
#[test_log::test(tokio::test)]
async fn heavy4_mempool_publish_fork_recovery_test(
    #[case] enable_full_validation: bool,
) -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var(
        "RUST_LOG",
        "debug,irys_actors::block_validation=off,storage::db::mdbx=off,reth=off,irys_p2p::server=off,irys_actors::mining=error",
    );
    }
    initialize_tracing();

    // config variables
    let num_blocks_in_epoch = 5; // test currently mines 4 blocks, and expects txs to remain in mempool
    let seconds_to_wait = 15;

    // setup config
    let block_migration_depth: u64 = num_blocks_in_epoch - 1;
    let mut a_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch.try_into().unwrap());
    a_config.consensus.get_mut().chunk_size = 32;
    a_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    a_config
        .consensus
        .get_mut()
        .enable_full_ingress_proof_validation = enable_full_validation;

    // signers
    // Create a signer (keypair) for the peer and fund it
    let b_signer = a_config.new_random_signer();
    let c_signer = a_config.new_random_signer();

    // uncomment for deterministic txs (and make b/c_signer mut)

    // b_signer.signer = SigningKey::from_slice(
    //     hex::decode("b360d276e1a5a26c59d46e5b12e0cec0f5166cb69552b5ba282a661bf2f8fe3e")?.as_slice(),
    // )?;
    // c_signer.signer = SigningKey::from_slice(
    //     hex::decode("ff235e7114eb975ca3bef4210db9aab2443c422497021e452f9dd4a327c562dc")?.as_slice(),
    // )?;

    a_config.fund_genesis_accounts(vec![&b_signer, &c_signer]);

    let a_node = IrysNodeTest::new_genesis(a_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    let a_signer = a_node.node_ctx.config.irys_signer();

    //  additional configs for peers
    let config_1 = a_node.testing_peer_with_signer(&c_signer);
    let config_2 = a_node.testing_peer_with_signer(&b_signer);

    // start peer nodes
    let b_node = IrysNodeTest::new(config_1)
        .start_and_wait_for_packing("NODE_B", seconds_to_wait)
        .await;
    let c_node = IrysNodeTest::new(config_2)
        .start_and_wait_for_packing("NODE_C", seconds_to_wait)
        .await;

    let mut network_height = 0;
    {
        // Post stake + pledge commitments to b
        let b_stake_tx = b_node.post_stake_commitment(None).await?; // zero() is the a block hash
        let b_pledge_tx = b_node.post_pledge_commitment(None).await?;

        // Post stake + pledge commitments to c
        let c_stake_tx = c_node.post_stake_commitment(None).await?;
        let c_pledge_tx = c_node.post_pledge_commitment(None).await?;

        // Wait for all commitment tx to show up in the node_a's mempool
        a_node
            .wait_for_mempool(b_stake_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(b_pledge_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_stake_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_pledge_tx.id(), seconds_to_wait)
            .await?;

        // Mine blocks to get the commitments included, epoch tasks performed, and assignments of partition_hash's to the peers
        a_node
            .mine_blocks(num_blocks_in_epoch.try_into().unwrap())
            .await?;
        network_height += num_blocks_in_epoch;

        // wait for migration to reach index height
        a_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Get the a nodes view of the peers assignments
        let b_assignments = a_node.get_partition_assignments(b_signer.address());
        let c_assignments = a_node.get_partition_assignments(c_signer.address());

        // Verify that one partition has been assigned to each peer to match its pledge
        assert_eq!(b_assignments.len(), 1);
        assert_eq!(c_assignments.len(), 1);

        // Wait for the peers to receive & process the epoch block
        b_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;
        c_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Wait for them to pack their storage modules with the partition_hashes
        b_node.wait_for_packing(seconds_to_wait).await;
        c_node.wait_for_packing(seconds_to_wait).await;
    }

    // check peer heights match a - i.e. that we are all in sync
    network_height = a_node.get_canonical_chain_height().await;

    b_node
        .wait_for_block_at_height(network_height, seconds_to_wait)
        .await?;
    c_node
        .wait_for_block_at_height(network_height, seconds_to_wait)
        .await?;

    // disable P2P/gossip
    a_node.gossip_disable();
    b_node.gossip_disable();
    c_node.gossip_disable();

    // A: create tx & mine block 1 (relative)

    let a_blk1_tx1 = a_node
        .post_data_tx(
            a_node.get_anchor().await?,
            [[1; 32], [1; 32], [1; 32]].concat(),
            &a_signer,
        )
        .await;

    a_node.upload_chunks(&a_blk1_tx1).await?;
    a_node
        .wait_for_ingress_proofs_no_mining(vec![a_blk1_tx1.header.id], seconds_to_wait)
        .await?;

    a_node.mine_block().await?;
    network_height += 1;

    let a_blk1 = a_node.get_block_by_height(network_height).await?;
    // check that a_blk1 contains a_blk1_tx1 in both publish and submit ledgers
    assert_eq!(
        a_blk1.data_ledgers[DataLedger::Submit].tx_ids,
        vec![a_blk1_tx1.header.id]
    );
    assert_eq!(
        a_blk1.data_ledgers[DataLedger::Publish].tx_ids,
        vec![a_blk1_tx1.header.id]
    );

    // B: mine block 1

    let b_blk1_tx1 = {
        let b_blk1_tx1 = b_node
            .post_data_tx(
                b_node.get_anchor().await?,
                [[2; 32], [2; 32], [2; 32]].concat(),
                &b_signer,
            )
            .await;

        b_node.upload_chunks(&b_blk1_tx1).await?;
        b_node
            .wait_for_ingress_proofs_no_mining(vec![b_blk1_tx1.header.id], seconds_to_wait)
            .await?;

        b_blk1_tx1
    };

    let (b_blk1, b_blk1_payload, b_blk1_txs) = b_node.mine_block_with_payload().await?;

    assert_eq!(
        b_blk1.data_ledgers[DataLedger::Submit].tx_ids,
        vec![b_blk1_tx1.header.id]
    );

    assert_eq!(
        b_blk1.data_ledgers[DataLedger::Publish].tx_ids,
        vec![b_blk1_tx1.header.id]
    );

    // B: Tx & Block 2

    // don't upload chunks, we want this in the submit ledger
    let b_blk2_tx1 = b_node
        .post_data_tx(
            b_node.get_anchor().await?,
            [[3; 32], [3; 32], [3; 32]].concat(),
            &b_signer,
        )
        .await;

    let (b_blk2, b_blk2_payload, b_blk2_txs) = {
        let result = b_node.mine_block_with_payload().await?;
        network_height += 1;
        result
    };

    assert_eq!(
        b_blk2.data_ledgers[DataLedger::Submit].tx_ids,
        vec![b_blk2_tx1.header.id]
    );
    assert_eq!(
        b_blk2.data_ledgers[DataLedger::Publish].tx_ids, // should not be promoted
        vec![]
    );

    b_node
        .wait_for_block_at_height(network_height, seconds_to_wait)
        .await?;
    b_node
        .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
        .await?;

    // send B1&2 to A, causing a reorg
    {
        let a1_b2_reorg_fut = a_node.wait_for_reorg(seconds_to_wait);

        // note we send the full blocks to node a, including txs, and chunks
        b_node
            .send_full_block(&a_node, &b_blk1, b_blk1_payload, b_blk1_txs)
            .await?;
        a_node
            .wait_for_block(&b_blk1.block_hash, seconds_to_wait)
            .await?;
        b_node
            .send_full_block(&a_node, &b_blk2, b_blk2_payload, b_blk2_txs)
            .await?;
        a_node
            .wait_for_block(&b_blk2.block_hash, seconds_to_wait)
            .await?;

        // wait for a reorg event
        let _a1_b2_reorg = a1_b2_reorg_fut.await?;
        a_node
            .wait_for_block_at_height(network_height, seconds_to_wait)
            .await?;
        assert_eq!(
            a_node.get_block_by_height(network_height).await?,
            b_node.get_block_by_height(network_height).await?
        );
    }

    // ensure mempool has settled to expected shape for submit/publish
    // (allow both A and B txs to appear)
    a_node
        .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
        .await?;

    // Wait for mempool to stabilize with expected shape after reorg
    a_node
        .wait_for_mempool_best_txs_shape(1, 1, 0, seconds_to_wait.try_into()?)
        .await?;

    let a_canonical_tip = a_node.get_canonical_chain().last().unwrap().block_hash();
    let a1_b2_reorg_mempool_txs = a_node.get_best_mempool_tx(a_canonical_tip).await?;

    // assert that a_blk1_tx1 is back in a's mempool
    assert_eq!(
        a1_b2_reorg_mempool_txs.submit_tx.len(),
        1,
        "We expected 1 submit tx from the mempool shape"
    );
    assert_eq!(
        a1_b2_reorg_mempool_txs.submit_tx[0].id, a_blk1_tx1.header.id,
        "Expected submit tx to be {:?}",
        a_blk1_tx1.header.id
    );

    assert_eq!(
        a1_b2_reorg_mempool_txs.publish_tx.txs.len(),
        1,
        "unexpected best mempool txs shape"
    );

    let a_blk1_tx1_proof1 = {
        let tx = a_node.node_ctx.db.tx()?;
        // Get the ingress proof from the database
        tx.get::<IngressProofs>(a_blk1_tx1.header.data_root)?
            .expect("Able to get a_blk1_tx1's ingress proof from DB")
    };

    let mut a_blk1_tx1_published = a_blk1_tx1.header.clone();
    a_blk1_tx1_published.set_promoted_height(None); // <- mark this tx as unpublished

    // assert that A’s tx is among publish candidates (treated as if it wasn't promoted)
    // (allow additional candidates as sometimes Bs tx will also show up)
    assert!(
        a1_b2_reorg_mempool_txs
            .publish_tx
            .txs
            .iter()
            .any(|h| h.id == a_blk1_tx1.header.id),
        "A's tx missing in publish candidates: got {:?}",
        a1_b2_reorg_mempool_txs
            .publish_tx
            .txs
            .iter()
            .map(|h| h.id)
            .collect::<Vec<_>>()
    );

    let a_blk1_tx1_mempool =
        {
            let (tx, rx) = oneshot::channel();
            a_node.node_ctx.service_senders.mempool.send_traced(
                MempoolServiceMessage::GetDataTxs(vec![a_blk1_tx1.header.id], tx),
            )?;
            let mempool_txs = rx.await?;
            let a_blk1_tx1_mempool = mempool_txs.first().unwrap().clone().unwrap();
            a_blk1_tx1_mempool
        };

    // ensure a_blk1_tx1 was orphaned back into the mempool, *without* an ingress proof
    // note: as [`get_publish_txs_and_proofs`] resolves ingress proofs, calling get_best_mempool_txs will return the header with an ingress proof.
    // so we have a separate path & assert to ensure the ingress proof is being removed when the tx is orphaned
    assert_eq!(
        a_blk1_tx1_mempool.id, a_blk1_tx1.header.id,
        "Transaction ID should match after reorg"
    );

    // gossip A's orphaned tx to B
    // get it ready for promotion, and then mine a block on B to include it

    b_node.post_data_tx_raw(&a_blk1_tx1.header).await;
    b_node.upload_chunks(&a_blk1_tx1).await?;
    b_node
        .wait_for_ingress_proofs_no_mining(vec![a_blk1_tx1.header.id], seconds_to_wait)
        .await?;

    // B: Mine B3
    let (b_blk3, b_blk3_payload, b_blk3_txs) = {
        let result = b_node.mine_block_with_payload().await?;
        network_height += 1;
        result
    };

    // ensure a_blk1_tx1 is included
    assert_eq!(
        b_blk3.data_ledgers[DataLedger::Submit].tx_ids,
        vec![a_blk1_tx1.header.id]
    );
    assert_eq!(
        b_blk3.data_ledgers[DataLedger::Publish].tx_ids, // should be promoted
        vec![a_blk1_tx1.header.id]
    );

    assert!(b_blk3.data_ledgers[DataLedger::Publish].proofs.is_some());

    // a_blk1_tx1 should have a new ingress proof (assert it's not the original from a_blk2)
    assert!(b_blk3.data_ledgers[DataLedger::Publish]
        .proofs
        .clone()
        .unwrap()
        .ne(&IngressProofsList(vec![a_blk1_tx1_proof1.proof.clone()])));

    // now we send (bypassing gossip) B3 back to A
    // it shouldn't reorg, and should accept the block
    // as well as overriding the ingress proof it has locally with the one from the block
    b_node
        .send_full_block(&a_node, &b_blk3, b_blk3_payload, b_blk3_txs)
        .await?;

    // wait for height and index on node a
    a_node
        .wait_for_block_at_height(network_height, seconds_to_wait)
        .await?;
    a_node
        .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
        .await?;

    assert_eq!(
        a_node.get_block_by_height(network_height).await?,
        b_node.get_block_by_height(network_height).await?
    );

    // Wait for the "best mempool txs" to settle to expected shape
    a_node
        .wait_for_mempool_best_txs_shape(0, 0, 0, seconds_to_wait.try_into()?)
        .await?;

    // (a second check) assert that nothing is in the mempool
    let a_canonical_tip = a_node.get_canonical_chain().last().unwrap().block_hash();
    let a_b_blk3_mempool_txs = a_node.get_best_mempool_tx(a_canonical_tip).await?;
    assert!(a_b_blk3_mempool_txs.submit_tx.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.txs.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.proofs.is_none());
    assert!(a_b_blk3_mempool_txs.commitment_tx.is_empty());

    // get a_blk1_tx1 from a, it should have b_blk3's ingress proof
    let a_blk1_tx1_b_blk3_tx1 = a_node
        .get_storage_tx_header_from_mempool(&a_blk1_tx1.header.id)
        .await?;

    assert_eq!(a_blk1_tx1_b_blk3_tx1.promoted_height(), Some(b_blk3.height));

    // gracefully shutdown nodes
    tokio::join!(a_node.stop(), b_node.stop(), c_node.stop(),);
    Ok(())
}

/// this test tests reorging in the context of confirmed commitment transactions
/// goals:
/// - ensure orphaned commitment transactions are returned to the mempool correctly
/// Steps:
/// create 2 nodes: A (genesis) B and C (C is currently unused)
/// mine commitments for B and C using A
/// make sure all the nodes are synchronised
/// prevent all gossip/P2P
///
/// send A a commitment Tx
/// send B a commitment Tx
/// prime a fork - mine one block on A and two on B
/// assert A and B's blocks include their respective commitment txs
/// trigger a reorg - gossip B's txs & blocks to A
/// assert that A has a reorg event
/// assert that A's tx is returned to the mempool
/// gossip A's tx to B
/// mine a block on B, assert A's tx is included correctly
/// gossip B's block back to A, assert that the commitment is no longer in best_mempool_txs

#[test_log::test(tokio::test)]
async fn heavy4_mempool_commitment_fork_recovery_test() -> eyre::Result<()> {
    // std::env::set_var(
    //     "RUST_LOG",
    //     "debug,irys_actors::block_validation=off,storage::db::mdbx=off,reth=off,irys_p2p::server=off,irys_actors::mining=error",
    // );
    // initialize_tracing();

    // config variables
    let num_blocks_in_epoch = 5; // test currently mines 4 blocks, and expects txs to remain in mempool
    let seconds_to_wait = 15;

    // setup config
    let block_migration_depth: u64 = num_blocks_in_epoch - 1;
    let mut a_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch.try_into().unwrap());
    a_config.consensus.get_mut().chunk_size = 32;
    a_config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    // signers
    // Create a signer (keypair) for the peer and fund it
    let b_signer = a_config.new_random_signer();
    let c_signer = a_config.new_random_signer();

    // uncomment for deterministic txs (and make b/c_signer mut)

    // b_signer.signer = SigningKey::from_slice(
    // hex::decode("b360d276e1a5a26c59d46e5b12e0cec0f5166cb69552b5ba282a661bf2f8fe3e")?.as_slice(),
    // )?;
    // c_signer.signer = SigningKey::from_slice(
    // hex::decode("ff235e7114eb975ca3bef4210db9aab2443c422497021e452f9dd4a327c562dc")?.as_slice(),
    // )?;

    a_config.fund_genesis_accounts(vec![&b_signer, &c_signer]);

    let a_node = IrysNodeTest::new_genesis(a_config.clone())
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // let a_signer = a_node.node_ctx.config.irys_signer();

    //  additional configs for peers
    let config_1 = a_node.testing_peer_with_signer(&c_signer);
    let config_2 = a_node.testing_peer_with_signer(&b_signer);

    // start peer nodes
    let b_node = IrysNodeTest::new(config_1)
        .start_and_wait_for_packing("NODE_B", seconds_to_wait)
        .await;
    let c_node = IrysNodeTest::new(config_2)
        .start_and_wait_for_packing("NODE_C", seconds_to_wait)
        .await;

    let mut network_height = 0;
    {
        // Post stake + pledge commitments to b
        let b_stake_tx = b_node.post_stake_commitment(None).await?;
        let b_pledge_tx = b_node.post_pledge_commitment(None).await?;

        // Post stake + pledge commitments to c
        let c_stake_tx = c_node.post_stake_commitment(None).await?;
        let c_pledge_tx = c_node.post_pledge_commitment(None).await?;

        // Wait for all commitment tx to show up in the node_a's mempool
        a_node
            .wait_for_mempool(b_stake_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(b_pledge_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_stake_tx.id(), seconds_to_wait)
            .await?;
        a_node
            .wait_for_mempool(c_pledge_tx.id(), seconds_to_wait)
            .await?;

        // Mine blocks to get the commitments included, epoch tasks performed, and assignments of partition_hash's to the peers
        a_node
            .mine_blocks(num_blocks_in_epoch.try_into().unwrap())
            .await?;
        network_height += num_blocks_in_epoch;

        // wait for migration to reach index height
        a_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Get the a nodes view of the peers assignments
        let b_assignments = a_node.get_partition_assignments(b_signer.address());
        let c_assignments = a_node.get_partition_assignments(c_signer.address());

        // Verify that one partition has been assigned to each peer to match its pledge
        assert_eq!(b_assignments.len(), 1);
        assert_eq!(c_assignments.len(), 1);

        // Wait for the peers to receive & process the epoch block
        b_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;
        c_node
            .wait_until_block_index_height(network_height - block_migration_depth, seconds_to_wait)
            .await?;

        // Wait for them to pack their storage modules with the partition_hashes
        b_node.wait_for_packing(seconds_to_wait).await;
        c_node.wait_for_packing(seconds_to_wait).await;
    }

    // check peer heights match a - i.e. that we are all in sync
    network_height = a_node.get_canonical_chain_height().await;

    b_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;
    c_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    // disable P2P/gossip
    {
        a_node.gossip_disable();
        b_node.gossip_disable();
        c_node.gossip_disable();
    }

    let _a_blk0 = a_node.get_block_by_height(network_height).await?;

    // A: create tx & mine block 1 (relative)

    let a_blk1_tx1 = a_node.post_pledge_commitment_without_gossip(None).await?;

    a_node.mine_block().await?;
    network_height += 1;

    let a_blk1 = a_node.get_block_by_height(network_height).await?;
    // check that a_blk1 contains a_blk1_tx1 in the SystemLedger
    assert_eq!(
        a_blk1.system_ledgers[SystemLedger::Commitment].tx_ids,
        vec![a_blk1_tx1.id()]
    );

    // B: mine block 1

    let b_blk1_tx1 = b_node.post_pledge_commitment_without_gossip(None).await?;

    let (b_blk1, b_blk1_payload, b_blk1_txs) = b_node.mine_block_with_payload().await?;
    // network_height += 1;

    // check that a_blk1 contains a_blk1_tx1 in the SystemLedger

    assert_eq!(
        b_blk1.system_ledgers[SystemLedger::Commitment].tx_ids,
        vec![b_blk1_tx1.id()]
    );

    // B: Tx & Block 2

    let b_blk2_tx1 = b_node.post_pledge_commitment_without_gossip(None).await?;

    let (b_blk2, b_blk2_payload, b_blk2_txs) = {
        let result = b_node.mine_block_with_payload().await?;
        network_height += 1;
        result
    };
    // check that a_blk1 contains a_blk1_tx1 in the SystemLedger

    assert_eq!(
        b_blk2.system_ledgers[SystemLedger::Commitment].tx_ids,
        vec![b_blk2_tx1.id()]
    );

    // // Gossip B1&2 to A, causing a reorg

    let a1_b2_reorg_fut = a_node.wait_for_reorg(seconds_to_wait);

    b_node
        .send_full_block(&a_node, &b_blk1, b_blk1_payload, b_blk1_txs)
        .await?;
    b_node
        .send_full_block(&a_node, &b_blk2, b_blk2_payload, b_blk2_txs)
        .await?;

    a_node
        .wait_for_block(&b_blk1.block_hash, seconds_to_wait)
        .await?;

    a_node
        .wait_for_block(&b_blk2.block_hash, seconds_to_wait)
        .await?;

    // wait for a reorg event

    let _a1_b2_reorg = a1_b2_reorg_fut.await?;

    a_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    assert_eq!(
        a_node.get_block_by_height(network_height).await?,
        b_node.get_block_by_height(network_height).await?
    );

    // Wait for mempool to stabilize with expected shape after reorg
    a_node
        .wait_for_mempool_best_txs_shape(0, 0, 1, seconds_to_wait.try_into()?)
        .await?;

    // assert that a_blk1_tx1 is back in a's mempool
    let a_canonical_tip = a_node.get_canonical_chain().last().unwrap().block_hash();
    let a1_b2_reorg_mempool_txs = a_node.get_best_mempool_tx(a_canonical_tip).await?;

    assert_eq!(
        a1_b2_reorg_mempool_txs.commitment_tx,
        vec![a_blk1_tx1.clone()]
    );

    // gossip A's orphaned tx to B
    b_node.post_commitment_tx(&a_blk1_tx1).await?;

    // B: Mine B3
    let (b_blk3, b_blk3_payload, b_blk3_txs) = {
        let result = b_node.mine_block_with_payload().await?;
        network_height += 1;
        result
    };

    // ensure a_blk1_tx1 is included
    assert_eq!(
        b_blk3.system_ledgers[SystemLedger::Commitment].tx_ids,
        vec![a_blk1_tx1.id()]
    );

    // now we gossip B3 back to A
    // it shouldn't reorg, and should accept the block

    b_node
        .send_full_block(&a_node, &b_blk3, b_blk3_payload, b_blk3_txs)
        .await?;

    a_node
        .wait_until_height(network_height, seconds_to_wait)
        .await?;

    assert_eq!(
        a_node.get_block_by_height(network_height).await?,
        b_node.get_block_by_height(network_height).await?
    );

    // assert that a_blk1_tx1 is no longer present in the mempool
    // (nothing should be in the mempool)
    let a_canonical_tip = a_node.get_canonical_chain().last().unwrap().block_hash();
    let a_b_blk3_mempool_txs = a_node.get_best_mempool_tx(a_canonical_tip).await?;
    assert!(a_b_blk3_mempool_txs.submit_tx.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.txs.is_empty());
    assert!(a_b_blk3_mempool_txs.publish_tx.proofs.is_none());
    assert!(a_b_blk3_mempool_txs.commitment_tx.is_empty());

    // tada!

    // gracefully shutdown nodes
    tokio::join!(a_node.stop(), b_node.stop(), c_node.stop(),);
    debug!("DONE!");
    Ok(())
}

// This test aims to (currently) test how the EVM interacts with forks and reorgs in the context of the mempool deciding which txs it should select
// it does this by:
// 1.) creating a fork with a transfer that would allow an account (recipient2) to afford a storage transaction (& validating this tx is included by the mempool)
// 2.) checking that the mempool function called for the block before this fork would prevent their transaction from being selected
// 3.) re-connecting the peers and ensuring that the correct fork was selected, and the account cannot afford the storage transaction (the funding tx was on the shorter fork)
// This test will probably be expanded in the future - it also includes a set of primitives for managing forks on the EVM/reth side too

#[tokio::test]
async fn heavy4_evm_mempool_fork_recovery_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var(
            "RUST_LOG",
            "debug,irys_actors::block_validation=none;irys_p2p::server=none;irys_actors::mining=error",
        );
    }
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().num_chunks_in_partition = 10;
    genesis_config
        .consensus
        .get_mut()
        .num_chunks_in_recall_range = 2;
    genesis_config.consensus.get_mut().num_partitions_per_slot = 1;
    genesis_config.storage.num_writes_before_sync = 1;
    genesis_config
        .consensus
        .get_mut()
        .entropy_packing_iterations = 1_000;
    genesis_config.consensus.get_mut().block_migration_depth = 1;

    // Create signers for the test accounts
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    let rich_account = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient1 = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient2 = IrysSigner::random_signer(&genesis_config.consensus_config());

    let chain_id = genesis_config.consensus_config().chain_id;

    // Fund genesis accounts for EVM transactions
    genesis_config.consensus.extend_genesis_accounts(vec![(
        rich_account.address(),
        GenesisAccount {
            balance: U256::from(100000000000000000000_u128), // 100 IRYS
            ..Default::default()
        },
    )]);

    // Fund the peer signers for network participation
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Setup Reth context for EVM transactions
    let genesis_reth_context = genesis.node_ctx.reth_node_adapter.clone();

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis.testing_peer_with_signer(&peer2_signer);

    // Start the peers
    let peer1 = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;

    let peer2 = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;

    // Setup Reth contexts for peers
    let peer1_reth_context = peer1.node_ctx.reth_node_adapter.clone();
    let peer2_reth_context = peer2.node_ctx.reth_node_adapter.clone();

    // ensure recipients have 0 balance
    let recipient1_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;
    let recipient2_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient2.address(), None)
        .await?;
    assert_eq!(recipient1_init_balance, U256::from(0));
    assert_eq!(recipient2_init_balance, U256::from(0));

    // need to stake & pledge peers before they can mine
    let post_wait_stake_commitment =
        async |peer: &IrysNodeTest<IrysNodeCtx>,
               genesis: &IrysNodeTest<IrysNodeCtx>|
               -> eyre::Result<(CommitmentTransaction, CommitmentTransaction)> {
            let stake_tx = peer.post_stake_commitment(None).await?;
            genesis
                .wait_for_mempool(stake_tx.id(), seconds_to_wait)
                .await?;
            let pledge_tx = peer.post_pledge_commitment(None).await?;
            genesis
                .wait_for_mempool(pledge_tx.id(), seconds_to_wait)
                .await?;
            Ok((stake_tx, pledge_tx))
        };

    post_wait_stake_commitment(&peer1, &genesis).await?;
    post_wait_stake_commitment(&peer2, &genesis).await?;

    // Mine a block to get the commitments included
    genesis.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    genesis.mine_block().await.unwrap();

    // Wait for peers to sync and start packing
    let _block_hash = peer1.wait_until_height(2, seconds_to_wait).await?;
    let _block_hash = peer2.wait_until_height(2, seconds_to_wait).await?;
    peer1.wait_for_packing(seconds_to_wait).await;
    peer2.wait_for_packing(seconds_to_wait).await;

    // Create EVM transactions that will be used in the fork scenario
    let rich_signer: LocalSigner<SigningKey> = rich_account.clone().into();

    // Transaction 1: Send to recipient1 (will be in peer1's fork)
    let evm_tx_req1 = TransactionRequest {
        to: Some(TxKind::Call(recipient1.alloy_address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(1)),
        nonce: Some(1),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let tx_env1 = TransactionTestContext::sign_tx(rich_signer.clone(), evm_tx_req1).await;
    let signed_tx1: Bytes = tx_env1.encoded_2718().into();

    // Transaction 2: Send to recipient2 (will be in peer2's fork)
    // TODO: remove manual nonce calculations (tricky when dealing with forks...)
    let evm_tx_req2 = TransactionRequest {
        to: Some(TxKind::Call(recipient2.alloy_address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(1000000000000000000_u128)),
        nonce: Some(1),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let tx_env2 = TransactionTestContext::sign_tx(rich_signer.clone(), evm_tx_req2).await;
    let signed_tx2: Bytes = tx_env2.encoded_2718().into();

    // Shared transaction that should be gossiped to all peers
    let shared_evm_tx_req = TransactionRequest {
        to: Some(TxKind::Call(recipient1.alloy_address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(123)),
        nonce: Some(0),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let shared_tx_env = TransactionTestContext::sign_tx(rich_signer, shared_evm_tx_req).await;
    let shared_signed_tx: Bytes = shared_tx_env.encoded_2718().into();

    // Inject the shared EVM transaction to genesis node (should gossip to peers)
    genesis_reth_context
        .rpc
        .inject_tx(shared_signed_tx)
        .await
        .expect("shared tx should be accepted");

    // mine a block
    let (_block, reth_exec_env, _block_txs) = genesis.mine_block_with_payload().await?;

    assert_eq!(reth_exec_env.block().transaction_count(), 1 + 1); // +1 for block reward

    let _block_hash = peer1.wait_until_height(3, seconds_to_wait).await?;
    let _block_hash = peer2.wait_until_height(3, seconds_to_wait).await?;

    // ensure the change is there
    let mut expected_recipient1_balance = U256::from(123);
    let mut expected_recipient2_balance = U256::from(0);

    let recipient1_balance = genesis_reth_context
        .rpc
        .get_balance(recipient1.address(), Some(BlockId::latest()))
        .await?;

    let recipient2_balance = genesis_reth_context
        .rpc
        .get_balance(recipient2.address(), None)
        .await?;

    assert_eq!(recipient1_balance, expected_recipient1_balance);
    assert_eq!(recipient2_balance, expected_recipient2_balance);

    // disconnect peers
    let genesis_peers = genesis.disconnect_all_reth_peers().await?;
    let peer1_peers = peer1.disconnect_all_reth_peers().await?;
    let peer2_peers = peer2.disconnect_all_reth_peers().await?;

    let wait_for_evm_tx = async |ctx: &IrysRethNodeAdapter, hash: &B256| -> eyre::Result<()> {
        // wait until the tx shows up
        let rpc = ctx.rpc_client().unwrap();
        loop {
            match EthApiClient::<TransactionRequest, Transaction, Block, Receipt, Header, Bytes>::transaction_by_hash(
                &rpc, *hash,
            )
            .await?
            {
                Some(_tx) => {
                    return Ok(());
                }
                None => sleep(Duration::from_millis(200)).await,
            }
        }
    };

    peer1_reth_context
        .rpc
        .inject_tx(signed_tx1.clone())
        .await
        .expect("peer1 tx should be accepted");

    wait_for_evm_tx(&peer1_reth_context, tx_env1.hash()).await?;

    expected_recipient1_balance += U256::from(1);

    peer2_reth_context
        .rpc
        .inject_tx(signed_tx2.clone())
        .await
        .expect("peer2 tx should be accepted");

    wait_for_evm_tx(&peer2_reth_context, tx_env2.hash()).await?;

    // Initial balance + received value
    expected_recipient2_balance += U256::from(1000000000000000000_u128);

    // Mine blocks on both peers in parallel to create a fork
    let (result1, result2) = tokio::join!(
        peer1.mine_blocks_without_gossip(1),
        peer2.mine_blocks_without_gossip(1)
    );

    // Fail the test on any error results
    result1?;
    result2?;

    // validate the peer blocks create forks with different EVM transactions

    let peer1_recipient1_balance = peer1_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;

    let peer1_recipient2_balance = peer1_reth_context
        .rpc
        .get_balance(recipient2.address(), None)
        .await?;

    let peer2_recipient1_balance = peer2_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;

    let peer2_recipient2_balance = peer2_reth_context
        .rpc
        .get_balance(recipient2.address(), None)
        .await?;

    // verify the fork
    assert_eq!(peer1_recipient1_balance, expected_recipient1_balance);
    assert_ne!(peer1_recipient2_balance, expected_recipient2_balance);

    assert_eq!(peer2_recipient2_balance, expected_recipient2_balance);
    assert_ne!(peer2_recipient1_balance, expected_recipient1_balance);

    // reconnect the peers
    genesis.reconnect_all_reth_peers(&genesis_peers);
    peer1.reconnect_all_reth_peers(&peer1_peers);
    peer2.reconnect_all_reth_peers(&peer2_peers);

    // try to insert a storage tx that is only valid on peer2's fork
    // then try to fetch best txs from peer2 once it reorgs to peer1's fork
    // which should fail/not include the TX

    let chunks = [[40; 32], [50; 32], [60; 32]];
    let data: Vec<u8> = chunks.concat();

    // Anchor this storage transaction to the actual genesis block hash instead of H256::zero()
    // Using H256::zero() encodes to base58 "1111.." which is now rejected as an invalid anchor.
    let _peer2_tx = peer2
        .post_data_tx_without_gossip(peer2.node_ctx.genesis_hash, data, &recipient2)
        .await;

    // call get best txs from the mempool

    let (tx, rx) = oneshot::channel();

    // Get the block hash at height - 1
    let previous_height = peer2.get_canonical_chain_height().await - 1;
    let previous_block_hash = peer2
        .get_block_by_height_from_index(previous_height, false)?
        .block_hash;

    peer2.node_ctx.service_senders.mempool.send_traced(
        MempoolServiceMessage::GetBestMempoolTxs(previous_block_hash, tx),
    )?;

    let best_previous = rx.await??;
    // previous block does not have the fund tx, the tx should not be present
    assert_eq!(
        best_previous.submit_tx.len(),
        0,
        "there should not be a storage tx (lack of funding due to changed parent Irys block)"
    );

    // Wait for mempool to stabilize after reconnection
    let (best_current_submit, _, _) = peer2
        .wait_for_mempool_best_txs_shape(1, 0, 0, seconds_to_wait.try_into()?)
        .await?;

    // latest block has the fund tx, so it should be present
    assert_eq!(best_current_submit.len(), 1, "There should be a storage tx");

    // mine another block on peer1, so it's the longest chain (with gossip)
    let height = peer1.get_canonical_chain_height().await;
    peer1.mine_block().await?;
    // peers should be able to sync
    let (gen, p2) = tokio::join!(
        genesis.wait_until_height(height + 1, 20),
        peer2.wait_until_height(height + 1, 20)
    );

    let _block_hash = gen?;
    let _block_hash = p2?;

    // the storage tx shouldn't be in the best mempool txs due to the fork change

    // Wait for mempool to stabilize after fork recovery
    let (best_current_submit, _, _) = peer2
        .wait_for_mempool_best_txs_shape(0, 0, 0, seconds_to_wait.try_into()?)
        .await?;

    assert_eq!(
        best_current_submit.len(),
        0,
        "There shouldn't be a storage tx"
    );

    tokio::join!(genesis.stop(), peer1.stop(), peer2.stop());

    Ok(())
}

#[tokio::test]
async fn heavy3_test_evm_gossip() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };
    initialize_tracing();

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().num_chunks_in_partition = 10;
    genesis_config
        .consensus
        .get_mut()
        .num_chunks_in_recall_range = 2;
    genesis_config.consensus.get_mut().num_partitions_per_slot = 1;
    genesis_config.storage.num_writes_before_sync = 1;
    genesis_config
        .consensus
        .get_mut()
        .entropy_packing_iterations = 1_000;
    genesis_config.consensus.get_mut().block_migration_depth = 1;

    // Create signers for the test accounts
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    let rich_account = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient1 = IrysSigner::random_signer(&genesis_config.consensus_config());
    let recipient2 = IrysSigner::random_signer(&genesis_config.consensus_config());

    let chain_id = genesis_config.consensus_config().chain_id;

    // Fund genesis accounts for EVM transactions
    genesis_config.consensus.extend_genesis_accounts(vec![(
        rich_account.address(),
        GenesisAccount {
            balance: U256::from(1000000000000000000_u128), // 1 IRYS
            ..Default::default()
        },
    )]);

    // Fund the peer signers for network participation
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Setup Reth context for EVM transactions
    let genesis_reth_context = genesis.node_ctx.reth_node_adapter.clone();

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis.testing_peer_with_signer(&peer2_signer);

    // Start the peers
    let peer1 = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;

    let peer2 = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;

    // Setup Reth contexts for peers
    let peer1_reth_context = peer1.node_ctx.reth_node_adapter.clone();
    let peer2_reth_context = peer2.node_ctx.reth_node_adapter.clone();

    // ensure recipients have 0 balance
    let recipient1_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;
    let recipient2_init_balance = genesis_reth_context
        .rpc
        .get_balance(recipient2.address(), None)
        .await?;
    assert_eq!(recipient1_init_balance, U256::from(0));
    assert_eq!(recipient2_init_balance, U256::from(0));

    // need to stake & pledge peers before they can mine
    let post_wait_stake_commitment =
        async |peer: &IrysNodeTest<IrysNodeCtx>,
               genesis: &IrysNodeTest<IrysNodeCtx>|
               -> eyre::Result<(CommitmentTransaction, CommitmentTransaction)> {
            let stake_tx = peer.post_stake_commitment(None).await?;
            genesis
                .wait_for_mempool(stake_tx.id(), seconds_to_wait)
                .await?;
            let pledge_tx = peer.post_pledge_commitment(None).await?;
            genesis
                .wait_for_mempool(pledge_tx.id(), seconds_to_wait)
                .await?;
            Ok((stake_tx, pledge_tx))
        };

    post_wait_stake_commitment(&peer1, &genesis).await?;
    post_wait_stake_commitment(&peer2, &genesis).await?;

    // Mine a block to get the commitments included
    genesis.mine_block().await.unwrap();

    // Mine another block to perform epoch tasks, and assign partition_hash's to the peers
    genesis.mine_block().await.unwrap();

    // Wait for peers to sync and start packing
    let _block_hash = peer1.wait_for_block_at_height(2, seconds_to_wait).await?;
    let _block_hash = peer2.wait_for_block_at_height(2, seconds_to_wait).await?;
    peer1.wait_for_packing(seconds_to_wait).await;
    peer2.wait_for_packing(seconds_to_wait).await;

    // Create EVM transactions that will be used in the fork scenario
    let rich_signer: LocalSigner<SigningKey> = rich_account.clone().into();

    // Shared transaction that should be gossiped to all peers
    let shared_evm_tx_req = TransactionRequest {
        to: Some(TxKind::Call(recipient1.alloy_address())),
        max_fee_per_gas: Some(20e9 as u128),
        max_priority_fee_per_gas: Some(20e9 as u128),
        gas: Some(21000),
        value: Some(U256::from(123)),
        nonce: Some(0),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let shared_tx_env = TransactionTestContext::sign_tx(rich_signer, shared_evm_tx_req).await;
    let shared_signed_tx: Bytes = shared_tx_env.encoded_2718().into();

    // Inject the shared EVM transaction to genesis node (should gossip to peers)
    genesis_reth_context
        .rpc
        .inject_tx(shared_signed_tx)
        .await
        .expect("shared tx should be accepted");

    peer1
        .wait_for_evm_tx(shared_tx_env.hash(), seconds_to_wait)
        .await?;
    peer2
        .wait_for_evm_tx(shared_tx_env.hash(), seconds_to_wait)
        .await?;

    let block = peer2.mine_block().await?;

    let evm_block_hash = block.evm_block_hash;

    genesis.wait_for_block(&block.block_hash, 20).await?;
    peer1.wait_for_block(&block.block_hash, 20).await?;

    // note: there is a delay between when the node says it has processed a block, and when all services (reth included) have actually processed it - so we wait

    peer2.wait_for_evm_block(evm_block_hash, 20).await?;

    peer1.wait_for_evm_block(evm_block_hash, 20).await?;

    let recipient1_balance = peer1_reth_context
        .rpc
        .get_balance(
            recipient1.address(),
            Some(BlockId::Hash(evm_block_hash.into())),
        )
        .await?;

    let recipient1_balance2 = peer1_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;

    let recipient1_balance3 = peer2_reth_context
        .rpc
        .get_balance(recipient1.address(), None)
        .await?;

    // assert reth "head" state is the state we expect on both peers
    assert_eq!(recipient1_balance, recipient1_balance2);
    assert_eq!(recipient1_balance, recipient1_balance3);

    assert_eq!(recipient1_balance, U256::from(123));

    let evm_block = peer1.get_evm_block_by_hash(evm_block_hash)?;
    let evm_block2 = peer1.get_evm_block_by_hash2(evm_block_hash).await?;
    assert_eq!(evm_block, evm_block2.into());

    assert_eq!(evm_block.body.transactions.len(), 2);

    tokio::join!(genesis.stop(), peer1.stop(), peer2.stop());

    Ok(())
}

#[test_log::test(tokio::test)]
/// send (staked) invalid pledge commitment txs where tx id has been tampered with
/// try with and without pending anchor
/// expect invalid txs to fail when sent directly to the mempool
async fn heavy_staked_pledge_commitment_tx_signature_validation_on_ingress_test() -> eyre::Result<()>
{
    let seconds_to_wait = 10;

    let mut genesis_config = NodeConfig::testing();

    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    //
    // Test case 1: Stake commitment txs
    //

    // create valid and invalid stake commitment tx
    let stake_anchor = genesis_node.get_anchor().await?;
    let stake_tx = new_stake_tx(&stake_anchor, &signer, &genesis_config.consensus_config());
    let mut stake_tx_invalid = stake_tx.clone();
    let mut bytes = stake_tx_invalid.id().to_fixed_bytes();
    bytes[0] ^= 0x01;
    stake_tx_invalid.set_id(H256::from(bytes));

    // ingest invalid stake tx directly to the mempool
    let res = genesis_node
        .ingest_commitment_tx(stake_tx_invalid.clone())
        .await
        .expect_err("expected failure but got success");
    match res {
        AddTxError::TxIngress(TxIngressError::InvalidSignature(address)) => {
            tracing::info!("Transaction from address {} failed to ingress with invalid signature, as expected!", address);
        }
        e => {
            panic!("Expected InvalidSignature but got: {:?}", e);
        }
    }

    // ingest valid stake tx
    genesis_node.ingest_commitment_tx(stake_tx.clone()).await?;

    //
    // Test case 2: staked pledge commitment txs
    //

    let mut tx_ids: Vec<H256> = vec![stake_tx.id()]; // txs used to check mempool ingress
    let pledge_tx = new_pledge_tx(
        &stake_anchor,
        &signer,
        &genesis_config.consensus_config(),
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
    )
    .await;

    // mine a block so we get some more anchors that are not pending
    genesis_node.mine_block().await?;

    let mut pledge_tx_invalid = pledge_tx.clone();
    let mut bytes = pledge_tx_invalid.id().to_fixed_bytes();
    bytes[1] ^= 0x01; // flip second bit to be different from pledge_tx_invalid_pending_anchor.id
    pledge_tx_invalid.set_id(H256::from(bytes));
    // check an invalid id on a pledge, that also has a valid non pending anchor
    let res = genesis_node
        .ingest_commitment_tx(pledge_tx_invalid.clone())
        .await
        .expect_err("expected failure but got success");
    match res {
        AddTxError::TxIngress(TxIngressError::InvalidSignature(address)) => {
            tracing::info!("Transaction from address {} failed to ingress with invalid signature, as expected!", address);
        }
        e => {
            panic!("Expected InvalidSignature but got: {:?}", e);
        }
    }
    genesis_node.ingest_commitment_tx(pledge_tx.clone()).await?;
    tx_ids.push(pledge_tx.id());

    // wait for all txs to ingress mempool
    genesis_node
        .wait_for_mempool_commitment_txs(tx_ids.clone(), seconds_to_wait)
        .await?;

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
/// send (unstaked) invalid pledge commitment txs where tx id has been tampered with
/// expect invalid txs to fail when sent directly to the mempool
async fn unheavy_staked_pledge_commitment_tx_signature_validation_on_ingress_test(
) -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();

    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    //
    // Test case 1: invalid pledge commitment txs
    //

    let mut pledge_tx_invalid = new_pledge_tx(
        &H256::zero(), // use genesis block for anchor
        &signer,
        &genesis_config.consensus_config(),
        genesis_node.node_ctx.mempool_pledge_provider.as_ref(),
    )
    .await;
    let mut bytes = pledge_tx_invalid.id().to_fixed_bytes();
    bytes[1] ^= 0x01; // flip second bit i.e. tamper with it
    pledge_tx_invalid.set_id(H256::from(bytes));
    // check an invalid id on a pledge
    let res = genesis_node
        .ingest_commitment_tx(pledge_tx_invalid.clone())
        .await
        .expect_err("expected failure but got success");
    assert!(
        matches!(
            res,
            AddTxError::TxIngress(TxIngressError::InvalidSignature(_))
        ),
        "Expected InvalidSignature for pledge with invalid id, got: {:?}",
        res
    );

    genesis_node.stop().await;

    Ok(())
}

#[test_log::test(tokio::test)]
/// try ingress invalid data tx where tx id has been tampered with
/// try ingress valid data tx where tx id has not been tampered with
/// expect invalid txs to fail when sent directly to the mempool
/// expect valid tx to ingress successfully
async fn data_tx_signature_validation_on_ingress_test() -> eyre::Result<()> {
    let seconds_to_wait = 10;

    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    // create a signed data transaction
    let valid_tx = genesis_node
        .create_signed_data_tx(&signer, b"hello".to_vec())
        .await
        .unwrap();

    // tamper with the transaction id
    let mut invalid_header = valid_tx.header.clone();
    let mut bytes = invalid_header.id.to_fixed_bytes();
    bytes[0] ^= 0x01;
    invalid_header.id = H256::from(bytes);

    // ingest invalid transaction directly to the mempool
    let res = genesis_node
        .ingest_data_tx(invalid_header.clone())
        .await
        .expect_err("expected failure but got success");
    assert!(
        matches!(
            res,
            AddTxError::TxIngress(TxIngressError::InvalidSignature(_))
        ),
        "Expected InvalidSignature but got: {:?}",
        res
    );

    // ingest valid transaction
    genesis_node.ingest_data_tx(valid_tx.header.clone()).await?;

    // wait for all txs to ingress mempool
    genesis_node
        .wait_for_mempool(valid_tx.header.id, seconds_to_wait)
        .await?;

    genesis_node.stop().await;

    Ok(())
}

/// Test mempool rejects stake transactions with invalid fee or value
#[rstest::rstest]
#[case::invalid_fee_less_than_required(
    |tx: &mut CommitmentTransaction, required_fee: u64, _required_value: irys_types::U256| {
        tx.set_fee(required_fee / 2); // 50 instead of 100
    },
)]
#[case::invalid_fee_zero(
    |tx: &mut CommitmentTransaction, _required_fee: u64, _required_value: irys_types::U256| {
        tx.set_fee(0);
    },
)]
#[case::invalid_value_less_than_required(
    |tx: &mut CommitmentTransaction, _required_fee: u64, required_value: irys_types::U256| {
        tx.set_value(required_value / irys_types::U256::from(2)); // 10000 instead of 20000
    },
)]
#[case::invalid_value_more_than_required(
    |tx: &mut CommitmentTransaction, _required_fee: u64, required_value: irys_types::U256| {
        tx.set_value(required_value + irys_types::U256::from(10000)); // 30000 instead of 20000
    },
)]
#[test_log::test(tokio::test)]
async fn stake_tx_fee_and_value_validation_test(
    #[case] tx_modifier: fn(&mut CommitmentTransaction, u64, irys_types::U256),
) -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    let config = &genesis_config.consensus_config();
    let required_fee = config.mempool.commitment_fee;
    let required_value = config.stake_value.amount;

    // Create stake transaction and apply the modifier
    let mut stake_tx = CommitmentTransaction::new_stake(config, genesis_node.get_anchor().await?);
    tx_modifier(&mut stake_tx, required_fee, required_value);
    signer.sign_commitment(&mut stake_tx)?;

    // Test that the transaction is rejected with the expected error
    let res = genesis_node
        .ingest_commitment_tx(stake_tx.clone())
        .await
        .expect_err("expected failure but got success");

    assert!(matches!(
        res,
        AddTxError::TxIngress(TxIngressError::CommitmentValidationError(_))
    ));

    genesis_node.stop().await;
    Ok(())
}

/// Test mempool validates pledge transaction fees and values correctly
#[rstest::rstest]
#[case::invalid_fee_less_than_required(
    0_u64, // pledge count
    |tx: &mut CommitmentTransaction, _config: &ConsensusConfig, _count: u64, required_fee: u64| {
        tx.set_fee(required_fee / 2); // 50 instead of 100
    },
)]
#[case::invalid_fee_zero(
    0_u64, // pledge count
    |tx: &mut CommitmentTransaction, _config: &ConsensusConfig, _count: u64, _required_fee: u64| {
        tx.set_fee(0);
    },
)]
#[case::invalid_value_too_low(
    0_u64, // pledge count
    |tx: &mut CommitmentTransaction, config: &ConsensusConfig, count: u64, _required_fee: u64| {
        let expected = CommitmentTransaction::calculate_pledge_value_at_count(config, count);
        tx.set_value(expected / irys_types::U256::from(2)); // Half the expected value
    },
)]
#[case::invalid_value_too_high(
    1_u64, // pledge count
    |tx: &mut CommitmentTransaction, config: &ConsensusConfig, count: u64, _required_fee: u64| {
        let expected = CommitmentTransaction::calculate_pledge_value_at_count(config, count);
        tx.set_value(expected * irys_types::U256::from(2)); // Double the expected value
    },
)]
#[case::invalid_value_wrong_count(
    2_u64, // pledge count
    |tx: &mut CommitmentTransaction, config: &ConsensusConfig, _count: u64, _required_fee: u64| {
        // Set value that would be correct for pledge count 0, but we're using count 2
        tx.set_value(CommitmentTransaction::calculate_pledge_value_at_count(config, 0));
    },
)]
#[test_log::test(tokio::test)]
async fn pledge_tx_fee_validation_test(
    #[case] pledge_count: u64,
    #[case] tx_modifier: fn(&mut CommitmentTransaction, &ConsensusConfig, u64, u64),
) -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    let config = &genesis_config.consensus_config();
    let required_fee = config.mempool.commitment_fee;

    // Create pledge transaction with modifications
    let mut pledge_tx = CommitmentTransaction::new_pledge(
        config,
        genesis_node.get_anchor().await?,
        &pledge_count,
        signer.address(),
    )
    .await;

    // Apply the modification (fee or value)
    tx_modifier(&mut pledge_tx, config, pledge_count, required_fee);
    signer.sign_commitment(&mut pledge_tx)?;

    // Test that the transaction is rejected with the expected error
    let res = genesis_node
        .ingest_commitment_tx(pledge_tx.clone())
        .await
        .expect_err("expected failure but got success");

    assert!(matches!(
        res,
        AddTxError::TxIngress(TxIngressError::CommitmentValidationError(_))
    ));
    genesis_node.stop().await;
    Ok(())
}

/// Test mempool accepts stake and pledge transactions with valid higher fees
#[rstest::rstest]
#[case::stake_double_fee(CommitmentTypeV1::Stake, 2)] // 200 instead of 100
#[case::stake_triple_fee(CommitmentTypeV1::Stake, 3)] // 300 instead of 100
#[case::stake_exact_fee(CommitmentTypeV1::Stake, 1)] // 100 (exact required fee)
#[case::pledge_double_fee(CommitmentTypeV1::Pledge {pledge_count_before_executing: 0 }, 2)] // First pledge, 200 instead of 100
#[case::pledge_triple_fee(CommitmentTypeV1::Pledge {pledge_count_before_executing: 1 }, 3)] // Second pledge, 300 instead of 100
#[case::pledge_exact_fee(CommitmentTypeV1::Pledge {pledge_count_before_executing: 0 }, 1)] // First pledge, 100 (exact required fee)
#[test_log::test(tokio::test)]
async fn heavy_commitment_tx_valid_higher_fee_test(
    #[case] commitment_type: CommitmentTypeV1,
    #[case] fee_multiplier: u64,
) -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    let config = &genesis_config.consensus_config();
    let required_fee = config.mempool.commitment_fee;

    // Create the appropriate transaction type with higher fee
    let mut commitment_tx = match commitment_type {
        CommitmentTypeV1::Stake => {
            CommitmentTransaction::new_stake(config, genesis_node.get_anchor().await?)
        }
        CommitmentTypeV1::Pledge {
            pledge_count_before_executing: count,
        } => {
            CommitmentTransaction::new_pledge(
                config,
                genesis_node.get_anchor().await?,
                &{ count },
                signer.address(),
            )
            .await
        }
        _ => unreachable!(),
    };

    // Apply the fee multiplier
    commitment_tx.set_fee(required_fee * fee_multiplier);
    signer.sign_commitment(&mut commitment_tx)?;

    // Should be accepted
    genesis_node
        .ingest_commitment_tx(commitment_tx.clone())
        .await?;

    // Wait for tx to appear in mempool
    genesis_node
        .wait_for_mempool_commitment_txs(vec![commitment_tx.id()], 10)
        .await?;

    genesis_node.stop().await;
    Ok(())
}

#[rstest::rstest]
#[case::stake_enough_balance(irys_types::U256::from(20000000000000000000100_u128 /* stake cost */), 1, 0)]
#[case::stake_not_enough_balance(irys_types::U256::from(0), 0, 0)]
#[case::pledge_15_enough_balance_for_1(
    irys_types::U256::from(20000000000000000000100_u128 /*stake cost*/ + 950000000000000000100_u128 /* pledge 1 */  ),
    2, // stake & 1 pledge
    15
)]
#[case::pledge_15_enough_balance_for_4(
    irys_types::U256::from(20000000000000000000100 /*stake cost*/ + 950000000000000000100_u128 /* pledge 1 */ + 509092394704739255819_u128 /* pledge 2 */ + 353439005113955754102_u128 /* pledge 3 */ + 272815859311795815354_u128 /* pledge 4 */  ),
    5, // stake & 4 pledges
    15
)]
#[test_log::test(tokio::test)]
/// Create a stake and some pledges
/// Determine the total cost
/// produce a block
/// see what txs get included (assert its count is equal to `initial_commitments` + 1)
/// transfer the user enough funds to afford the remaining commitments
/// produce another block, make sure it includes the rest
async fn heavy_commitment_tx_cumulative_fee_validation_test(
    #[case] starting_balance: irys_types::U256,
    #[case] initial_commitments: u64,
    #[case] total_pledge_count: u64,
) -> eyre::Result<()> {
    use std::collections::HashSet;

    let mut genesis_config = NodeConfig::testing();
    let signer = genesis_config.new_random_signer();
    let rich_signer = genesis_config.new_random_signer();

    genesis_config.consensus.extend_genesis_accounts([
        (
            signer.address(),
            GenesisAccount {
                balance: starting_balance.into(),
                ..Default::default()
            },
        ),
        (
            rich_signer.address(),
            GenesisAccount {
                balance: U256::MAX,
                ..Default::default()
            },
        ),
    ]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;

    let config = &genesis_config.consensus_config();

    // create the stake & pledges
    // compute the fee total
    let mut stake = CommitmentTransaction::new_stake(config, genesis_node.get_anchor().await?);
    signer.sign_commitment(&mut stake)?;

    let mut fee_total = stake.total_cost();
    info!(
        "stake cost: {}, starting balance: {}",
        &fee_total, &starting_balance
    );
    let can_afford_stake = starting_balance >= stake.total_cost();

    let mut first_block_commitments = HashSet::new();
    let mut third_block_commitments = HashSet::new();

    if can_afford_stake {
        first_block_commitments.insert(stake.id());
    } else {
        third_block_commitments.insert(stake.id());
    }

    let mut commitments = vec![stake];

    for i in 0..total_pledge_count {
        let mut pledge_tx = CommitmentTransaction::new_pledge(
            config,
            genesis_node.get_anchor().await?,
            &i,
            signer.address(),
        )
        .await;

        signer.sign_commitment(&mut pledge_tx)?;
        fee_total += pledge_tx.total_cost();
        info!(
            "needed for stake & {} pledges: {} (pledge: {})",
            &i + 1,
            &fee_total,
            &pledge_tx.total_cost()
        );
        if fee_total <= starting_balance {
            first_block_commitments.insert(pledge_tx.id());
        } else {
            third_block_commitments.insert(pledge_tx.id());
        }
        commitments.push(pledge_tx);
    }

    assert_eq!(first_block_commitments.len(), initial_commitments as usize);

    for commitment in commitments.iter() {
        //ingest as gossip so we bypass the initial fund checks
        genesis_node.gossip_commitment_to_node(commitment).await?
    }

    // mine a block
    // the commitments we know we can afford should be in this block
    let block = genesis_node.mine_block().await?;

    // check that the correct commitments were included
    let block_tx_ids: HashSet<H256> = block
        .system_ledgers
        .iter()
        .find(|ledger| ledger.ledger_id == SystemLedger::Commitment as u32)
        .map(|ledger| HashSet::from_iter(ledger.tx_ids.0.iter().copied()))
        .unwrap_or_else(HashSet::new);

    let diff1 = block_tx_ids
        .difference(&first_block_commitments)
        .collect::<Vec<_>>();
    assert!(diff1.is_empty());

    // mine a new block with a fund transfer tx

    let remaining_fees = fee_total.saturating_sub(starting_balance);

    let reth_context = genesis_node.node_ctx.reth_node_adapter.clone();

    let evm_tx_req = TransactionRequest {
        to: Some(TxKind::Call(signer.address().into())),
        max_fee_per_gas: Some(20_000_000_000), //20 gwei
        max_priority_fee_per_gas: Some(20_000_000_000),
        gas: Some(21_000),
        value: Some(remaining_fees.into()),
        nonce: Some(0),
        chain_id: Some(config.chain_id),
        ..Default::default()
    };
    let tx_env = TransactionTestContext::sign_tx(rich_signer.clone().into(), evm_tx_req).await;

    let _evm_tx_hash = reth_context
        .rpc
        .inject_tx(tx_env.encoded_2718().into())
        .await
        .expect("tx should be accepted");

    // check that the users's balance has increased
    let old_balance: irys_types::U256 = reth_context
        .rpc
        .get_balance(signer.address(), None)
        .await?
        .into();

    let block2 = genesis_node.mine_block().await?;

    let new_balance: irys_types::U256 = reth_context
        .rpc
        .get_balance(signer.address(), None)
        .await?
        .into();

    assert!(new_balance == old_balance + remaining_fees);

    // ensure that the rest of the commitments are in the next block
    let block2_tx_ids: HashSet<H256> = block2
        .system_ledgers
        .iter()
        .find(|ledger| ledger.ledger_id == SystemLedger::Commitment as u32)
        .map(|ledger| HashSet::from_iter(ledger.tx_ids.0.iter().copied()))
        .unwrap_or_else(HashSet::new);

    assert!(block2_tx_ids.is_empty());

    // now we have sufficient funds
    let block3 = genesis_node.mine_block().await?;

    // ensure that the rest of the commitments are in this block
    let block3_tx_ids: HashSet<H256> = block3
        .system_ledgers
        .iter()
        .find(|ledger| ledger.ledger_id == SystemLedger::Commitment as u32)
        .map(|ledger| HashSet::from_iter(ledger.tx_ids.0.iter().copied()))
        .unwrap_or_else(HashSet::new);

    assert!(block3_tx_ids
        .difference(&third_block_commitments)
        .collect::<Vec<_>>()
        .is_empty());

    assert_eq!(
        first_block_commitments.len() + third_block_commitments.len(),
        (total_pledge_count + 1) as usize
    ); // +1 for stake

    genesis_node.stop().await;
    Ok(())
}
