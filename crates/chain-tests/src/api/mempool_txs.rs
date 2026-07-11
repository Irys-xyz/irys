//! Integration tests for `GET /v1/mempool/txs`.

use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_actors::mempool_service::MempoolPendingTxs;
use irys_types::{IrysTransactionResponse, NodeConfig, irys::IrysSigner};
use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::sleep;

async fn fetch_mempool_txs(
    client: &reqwest::Client,
    base: &str,
    limit: Option<usize>,
    after_id: Option<&str>,
) -> eyre::Result<(StatusCode, MempoolPendingTxs)> {
    let mut params = Vec::new();
    if let Some(n) = limit {
        params.push(format!("limit={n}"));
    }
    if let Some(a) = after_id {
        params.push(format!("after_id={a}"));
    }
    let url = if params.is_empty() {
        format!("{base}/v1/mempool/txs")
    } else {
        format!("{base}/v1/mempool/txs?{}", params.join("&"))
    };
    let response = client.get(&url).send().await?;
    let status = response.status();
    let body = response.json::<MempoolPendingTxs>().await?;
    Ok((status, body))
}

#[test_log::test(tokio::test)]
async fn heavy_test_mempool_txs_empty_and_pending_lifecycle() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(10_000_000_000_000_000_000_u128),
            ..Default::default()
        },
    )]);
    // Fast confirmation so included_height lands quickly after mine.
    config.consensus.get_mut().block_migration_depth = 1;

    let node = IrysNodeTest::new_genesis(config).start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    let base = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    // 1. Empty mempool → 200 + empty arrays + zero counts (not 404).
    let (status, empty) = fetch_mempool_txs(&client, &base, None, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.data_txs.is_empty());
    assert!(empty.commitment_txs.is_empty());
    assert_eq!(empty.data_tx_count, 0);
    assert_eq!(empty.commitment_tx_count, 0);
    assert!(!empty.truncated);
    assert!(empty.total_data_tx_count.is_none());

    // Status endpoint still works and is independent of the list.
    let status_resp = client
        .get(format!("{base}/v1/mempool/status"))
        .send()
        .await?;
    assert_eq!(status_resp.status(), StatusCode::OK);

    // 2. Submit a data tx → id appears; counts match.
    let data = vec![1_u8; 64];
    let data_size = data.len() as u64;
    let anchor = node.get_anchor().await?;
    let tx = node.post_data_tx(anchor, data, &signer).await;
    let tx_id = tx.header.id;
    node.wait_for_mempool(tx_id, 10).await?;

    let (status, pending) = fetch_mempool_txs(&client, &base, None, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending.data_tx_count, pending.data_txs.len());
    assert_eq!(pending.commitment_tx_count, pending.commitment_txs.len());
    assert!(
        pending.data_txs.iter().any(|t| t.id == tx_id),
        "posted data tx should appear in mempool list: {pending:?}"
    );
    let entry = pending
        .data_txs
        .iter()
        .find(|t| t.id == tx_id)
        .expect("entry");
    assert_eq!(entry.byte_size, data_size);
    assert!(entry.chunks >= 1);
    assert_eq!(entry.data_root, tx.header.data_root);

    // 2b. Cursor paging reaches every data tx past `limit`.
    for i in 0..4_u8 {
        let anchor = node.get_anchor().await?;
        // Distinct payload → distinct id (identical bytes can collapse).
        let extra = node
            .post_data_tx(anchor, vec![100 + i; 64 + i as usize], &signer)
            .await;
        node.wait_for_mempool(extra.header.id, 10).await?;
    }
    let (_, full) = fetch_mempool_txs(&client, &base, Some(500), None).await?;
    let full_ids: std::collections::BTreeSet<_> = full.data_txs.iter().map(|t| t.id).collect();
    assert!(
        full_ids.len() >= 5,
        "expected >=5 pending data txs before paging, got {}",
        full_ids.len()
    );

    let mut paged_ids: Vec<irys_types::H256> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..(full_ids.len() + 5) {
        let (st, page) = fetch_mempool_txs(&client, &base, Some(2), cursor.as_deref()).await?;
        assert_eq!(st, StatusCode::OK);
        assert!(page.data_txs.len() <= 2);
        if page.data_txs.is_empty() {
            break;
        }
        paged_ids.extend(page.data_txs.iter().map(|t| t.id));
        cursor = Some(page.data_txs.last().unwrap().id.to_string());
        if !page.truncated {
            break;
        }
    }
    let mut ascending = paged_ids.clone();
    ascending.sort();
    ascending.dedup();
    assert_eq!(paged_ids, ascending, "pages must be ascending, no dups");
    let paged_set: std::collections::BTreeSet<_> = paged_ids.into_iter().collect();
    assert_eq!(
        paged_set, full_ids,
        "cursor must reach every pending data tx"
    );

    let bad = client
        .get(format!("{base}/v1/mempool/txs?after_id=not-a-valid-id"))
        .send()
        .await?;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // 2c. GET /v1/tx/{id} succeeds for a listed pending id.
    let tx_resp = client.get(format!("{base}/v1/tx/{tx_id}")).send().await?;
    assert_eq!(tx_resp.status(), StatusCode::OK);
    let fetched: IrysTransactionResponse = tx_resp.json().await?;
    match fetched {
        IrysTransactionResponse::Storage(header) => {
            assert_eq!(header.id, tx_id);
        }
        IrysTransactionResponse::Commitment(_) => {
            panic!("expected storage tx for listed data id");
        }
    }

    // 3. After inclusion → id leaves the list on subsequent poll.
    node.mine_block().await?;
    node.wait_for_tx_included(&tx_id, 15).await?;

    // Poll until the list drops the tx (BlockConfirmed is async).
    let mut left = false;
    for _ in 0..50 {
        let (_, after) = fetch_mempool_txs(&client, &base, None, None).await?;
        if !after.data_txs.iter().any(|t| t.id == tx_id) {
            left = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        left,
        "included data tx should leave the pending list after confirmation"
    );

    // Repeated polls stay functional (no wall-clock bound — flakes under load).
    for _ in 0..3 {
        let (status, _) = fetch_mempool_txs(&client, &base, Some(100), None).await?;
        assert_eq!(status, StatusCode::OK);
    }

    // Status counts still present after list usage (compat).
    let status_json: serde_json::Value = client
        .get(format!("{base}/v1/mempool/status"))
        .send()
        .await?
        .json()
        .await?;
    assert!(status_json.get("data_tx_count").is_some());
    assert!(status_json.get("commitment_tx_count").is_some());
    assert!(status_json.get("pending_chunks_count").is_some());

    node.stop().await;
    Ok(())
}
