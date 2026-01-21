//! api client tests

use crate::utils::IrysNodeTest;
use irys_api_client::ApiClientExt as _;
use irys_api_client::{ApiClient as _, IrysApiClient, TransactionStatus};
use irys_chain::IrysNodeCtx;
use irys_types::{BlockIndexQuery, IrysTransactionResponse, NodeConfig};
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr as _,
};
use tracing::debug;

async fn check_get_block_index_endpoint(
    api_client: &IrysApiClient,
    api_address: SocketAddr,
    _ctx: &IrysNodeTest<IrysNodeCtx>,
) {
    api_client
        .get_block_index(
            api_address,
            BlockIndexQuery {
                height: 0,
                limit: 100,
            },
        )
        .await
        .expect("valid get block index response");
}

async fn check_transaction_endpoints(
    api_client: &IrysApiClient,
    api_address: SocketAddr,
    ctx: &IrysNodeTest<IrysNodeCtx>,
) {
    // advance one block
    let _previous_header = ctx.mine_block().await.expect("expected mined block");
    // advance one block, finalizing the previous block
    let _header = ctx.mine_block().await.expect("expected mined block");

    let tx = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![1, 2, 3])
        .await
        .unwrap();
    let tx_id = tx.header.id;
    let tx_2 = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![4, 5, 6])
        .await
        .unwrap();
    let tx_2_id = tx_2.header.id;

    // This method doesn't return anything if there's no error
    api_client
        .post_transaction(api_address, tx.header.clone())
        .await
        .expect("valid post transaction response");
    api_client
        .post_transaction(api_address, tx_2.header.clone())
        .await
        .expect("valid post transaction response");

    // advance one block to add the transaction to the block
    let _header = ctx.mine_block().await.expect("expected mined block");

    let retrieved_tx = api_client
        .get_transaction(api_address, tx_id)
        .await
        .expect("valid get transaction response");

    let storage_header = match retrieved_tx {
        IrysTransactionResponse::Storage(header) => header,
        _ => panic!("expected storage transaction response"),
    };

    assert!(storage_header.eq_tx(&tx.header));

    let txs = api_client
        .get_transactions(api_address, &[tx_id, tx_2_id])
        .await
        .expect("valid get transactions response");

    assert_eq!(txs.len(), 2);
    assert!(txs.contains(&IrysTransactionResponse::Storage(tx.header)));
    assert!(txs.contains(&IrysTransactionResponse::Storage(tx_2.header)));
}

async fn check_get_block_endpoint(
    api_client: &IrysApiClient,
    api_address: SocketAddr,
    ctx: &IrysNodeTest<IrysNodeCtx>,
) {
    // advance one block
    let previous_header = ctx.mine_block().await.expect("expected mined block");
    // advance one block, finalizing the previous block
    let _header = ctx.mine_block().await.expect("expected mined block");

    let previous_block_hash = previous_header.block_hash;
    let block = api_client
        .get_block_by_hash(api_address, previous_block_hash, true)
        .await
        .expect("valid get block response");

    assert!(block.is_some());
    debug!("block: {:?}", block);
}

async fn check_info_endpoint(
    api_client: &IrysApiClient,
    api_address: SocketAddr,
    ctx: &IrysNodeTest<IrysNodeCtx>,
) {
    let info = api_client
        .node_info(api_address)
        .await
        .expect("valid get info response");

    assert_eq!(info.chain_id, ctx.node_ctx.config.consensus.chain_id);
}

#[test_log::test(tokio::test)]
async fn heavy_api_client_all_endpoints_should_work() {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new();

    check_transaction_endpoints(&api_client, api_address, &ctx).await;
    check_get_block_endpoint(&api_client, api_address, &ctx).await;
    check_get_block_index_endpoint(&api_client, api_address, &ctx).await;
    check_info_endpoint(&api_client, api_address, &ctx).await;

    ctx.stop().await;
}

/// Ensures wait_for_promotion returns an error when the tx was never posted or is otherwise missing.
/// Guards against silent success on NOT_FOUND or invalid responses from /tx/{id}/promotion_status.
#[test_log::test(tokio::test)]
async fn api_client_wait_for_promotion_errors_for_missing_tx() {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new();

    // Create a tx but do NOT post it; waiting for promotion should error out
    let tx = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![7, 8, 9])
        .await
        .unwrap();

    let result = api_client
        .wait_for_promotion(api_address, tx.header.id, 3)
        .await;

    assert!(
        result.is_err(),
        "wait_for_promotion should error for a missing/unposted tx"
    );

    ctx.stop().await;
}

/// Ensures wait_for_promotion succeeds for a properly posted tx after uploading chunks and mining.
/// Guards against regressions in /tx/{id}/promotion_status and client polling behavior.
#[test_log::test(tokio::test)]
async fn api_client_wait_for_promotion_happy_path() {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new();

    // Create a full data tx, upload chunks, and post header
    let tx = ctx
        .create_signed_data_tx(
            &ctx.node_ctx.config.irys_signer(),
            vec![1, 2, 3, 4, 5, 6, 7, 8],
        )
        .await
        .unwrap();

    // Upload chunks first so the node has the data
    api_client
        .upload_chunks(api_address, &tx)
        .await
        .expect("upload_chunks should succeed");

    // Post the transaction header
    api_client
        .post_transaction(api_address, tx.header.clone())
        .await
        .expect("post_transaction should succeed");

    // Optionally advance a block to drive background processing
    let _ = ctx.mine_block().await.expect("expected mined block");

    // This should succeed if promotion occurs within the attempts window
    api_client
        .wait_for_promotion(api_address, tx.header.id, 100)
        .await
        .expect("wait_for_promotion should succeed for a properly posted tx");

    ctx.stop().await;
}

/// Tests the transaction status API lifecycle: PENDING -> MINED -> FINALIZED
#[test_log::test(tokio::test)]
async fn api_tx_status_lifecycle() {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new();

    // Create and post a transaction
    let tx = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![1, 2, 3])
        .await
        .unwrap();
    let tx_id = tx.header.id;

    api_client
        .post_transaction(api_address, tx.header.clone())
        .await
        .expect("post_transaction should succeed");

    // Check status - should be PENDING
    let status = api_client
        .get_transaction_status(api_address, tx_id)
        .await
        .expect("get_transaction_status should succeed")
        .expect("status should exist");

    assert!(matches!(status.status, TransactionStatus::Pending));
    assert!(status.block_height.is_none());
    assert!(status.confirmations.is_none());

    // Mine a block to include the transaction
    ctx.mine_block().await.expect("expected mined block");

    // Poll until transaction is MINED or FINALIZED (accept both as terminal states)
    let mut status = None;
    for _ in 0..25 {
        let s = api_client
            .get_transaction_status(api_address, tx_id)
            .await
            .expect("get_transaction_status should succeed")
            .expect("status should exist");

        if matches!(s.status, TransactionStatus::Pending) {
            ctx.mine_block().await.expect("expected mined block");
            continue;
        }

        // Accept both Mined and Finalized as valid terminal states
        if matches!(
            s.status,
            TransactionStatus::Mined | TransactionStatus::Finalized
        ) {
            status = Some(s);
            break;
        }
    }

    let status = status.expect("transaction should eventually be mined or finalized");
    assert!(matches!(
        status.status,
        TransactionStatus::Mined | TransactionStatus::Finalized
    ));
    assert!(status.block_height.is_some());
    assert!(status.confirmations.is_some());

    let included_height = status
        .block_height
        .expect("included status should have block_height");
    let migration_depth = ctx.node_ctx.config.consensus.block_migration_depth as u64;

    // Mine more blocks to reach migration depth and make it CONFIRMED
    // Skip if already Finalized
    if matches!(status.status, TransactionStatus::Mined) {
        for _ in 0..(migration_depth + 2) {
            ctx.mine_block().await.expect("expected mined block");
        }
    }

    ctx.wait_until_block_index_height(included_height, 15)
        .await
        .expect("block index should reach included tx height");

    // Check status - should be CONFIRMED
    let status = api_client
        .get_transaction_status(api_address, tx_id)
        .await
        .expect("get_transaction_status should succeed")
        .expect("status should exist");

    assert!(matches!(status.status, TransactionStatus::Finalized));
    assert!(status.block_height.is_some());
    assert!(status.confirmations.is_some());

    ctx.stop().await;
}

/// Tests transaction status for commitment transactions
#[test_log::test(tokio::test)]
async fn api_tx_status_commitment_tx() {
    let config = NodeConfig::testing();
    let ctx = IrysNodeTest::new_genesis(config).start().await;
    ctx.wait_for_packing(20).await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.bind_port,
    );
    let api_client = IrysApiClient::new();

    // Create and post a valid pledge commitment transaction.
    // NOTE: stake commitments can legitimately be skipped for inclusion when the signer is already staked,
    // which would leave the tx PENDING forever and make this test flaky.
    let anchor = ctx.get_anchor().await.expect("expected anchor");
    let signer = ctx.node_ctx.config.irys_signer();
    let mut pledge_tx = irys_types::CommitmentTransaction::new_pledge(
        &ctx.node_ctx.config.consensus,
        anchor,
        ctx.node_ctx.mempool_pledge_provider.as_ref(),
        signer.address(),
    )
    .await;
    signer
        .sign_commitment(&mut pledge_tx)
        .expect("expected pledge tx to sign");
    let tx_id = pledge_tx.id();

    api_client
        .post_commitment_transaction(api_address, pledge_tx.clone())
        .await
        .expect("post_commitment_transaction should succeed");

    // Check status - should be PENDING
    let status = api_client
        .get_transaction_status(api_address, tx_id)
        .await
        .expect("get_transaction_status should succeed")
        .expect("status should exist");

    assert!(matches!(status.status, TransactionStatus::Pending));

    // Commitment txs may not land in the immediately-next block; mine/poll until included.
    let mut status = None;
    for _ in 0..25 {
        let s = api_client
            .get_transaction_status(api_address, tx_id)
            .await
            .expect("get_transaction_status should succeed")
            .expect("status should exist");

        if matches!(s.status, TransactionStatus::Pending) {
            ctx.mine_block().await.expect("expected mined block");
            continue;
        }

        status = Some(s);
        break;
    }

    let status = status.expect("commitment tx should eventually be included");
    assert!(
        matches!(
            status.status,
            TransactionStatus::Mined | TransactionStatus::Finalized
        ),
        "unexpected status: {:?}",
        status
    );
    assert!(status.block_height.is_some());
    assert!(status.confirmations.is_some());

    ctx.stop().await;
}
