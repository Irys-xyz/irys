//! Integration tests for `GET /v1/mempool/txs`.

use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_actors::mempool_service::MempoolPendingTxs;
use irys_types::{H256, IrysTransactionResponse, NodeConfig, irys::IrysSigner};
use reqwest::StatusCode;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::time::sleep;

async fn fetch_mempool_txs(
    client: &reqwest::Client,
    base: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
) -> eyre::Result<(StatusCode, MempoolPendingTxs)> {
    let mut params = Vec::new();
    if let Some(n) = limit {
        params.push(format!("limit={n}"));
    }
    if let Some(c) = cursor {
        // Cursor is base58-safe; no extra encoding required.
        params.push(format!("cursor={c}"));
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

/// Drain all pages via `next_cursor`; assert no dups and stable ascending ids.
async fn collect_all_pages(
    client: &reqwest::Client,
    base: &str,
    limit: usize,
) -> eyre::Result<(Vec<H256>, Vec<H256>)> {
    let mut data_ids = Vec::new();
    let mut commit_ids = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..64 {
        let (st, page) = fetch_mempool_txs(client, base, Some(limit), cursor.as_deref()).await?;
        assert_eq!(st, StatusCode::OK);
        assert!(page.data_txs.len() <= limit);
        assert!(page.commitment_txs.len() <= limit);

        data_ids.extend(page.data_txs.iter().map(|t| t.id));
        commit_ids.extend(page.commitment_txs.iter().map(|t| t.id));

        if !page.truncated {
            assert!(page.next_cursor.is_none());
            break;
        }
        let next = page
            .next_cursor
            .expect("truncated pages must return next_cursor");
        cursor = Some(next);
    }

    let mut data_sorted = data_ids.clone();
    data_sorted.sort();
    data_sorted.dedup();
    assert_eq!(
        data_ids, data_sorted,
        "data pages must be ascending, no dups"
    );

    let mut commit_sorted = commit_ids.clone();
    commit_sorted.sort();
    commit_sorted.dedup();
    assert_eq!(
        commit_ids, commit_sorted,
        "commitment pages must be ascending, no dups"
    );

    Ok((data_ids, commit_ids))
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
    assert!(empty.next_cursor.is_none());
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
        let extra = node
            .post_data_tx(anchor, vec![100 + i; 64 + i as usize], &signer)
            .await;
        node.wait_for_mempool(extra.header.id, 10).await?;
    }
    let (_, full) = fetch_mempool_txs(&client, &base, Some(500), None).await?;
    let full_ids: BTreeSet<_> = full.data_txs.iter().map(|t| t.id).collect();
    assert!(
        full_ids.len() >= 5,
        "expected >=5 pending data txs before paging, got {}",
        full_ids.len()
    );

    let (paged_data, _) = collect_all_pages(&client, &base, 2).await?;
    let paged_set: BTreeSet<_> = paged_data.into_iter().collect();
    assert_eq!(
        paged_set, full_ids,
        "cursor must reach every pending data tx"
    );

    let bad = client
        .get(format!("{base}/v1/mempool/txs?cursor=not-a-valid-cursor"))
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

    for _ in 0..3 {
        let (status, _) = fetch_mempool_txs(&client, &base, Some(100), None).await?;
        assert_eq!(status, StatusCode::OK);
    }

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

/// Mixed data + commitment pool: dual-list cursor pages both without dups/skips
/// even when ids interleave and lists empty at different rates.
#[test_log::test(tokio::test)]
async fn heavy_test_mempool_txs_mixed_type_cursor_paging() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    let data_signer = IrysSigner::random_signer(&config.consensus_config());
    let stake_signers: Vec<_> = (0..3)
        .map(|_| IrysSigner::random_signer(&config.consensus_config()))
        .collect();
    // Stake value is large; use fund helper (not a tiny test balance).
    let mut all_signers = vec![&data_signer];
    all_signers.extend(stake_signers.iter());
    config.fund_genesis_accounts(all_signers);
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

    // Interleave posts: data and stakes so both lists are non-empty together.
    let mut expected_data = BTreeSet::new();
    let mut expected_commit = BTreeSet::new();
    for i in 0..3_u8 {
        let anchor = node.get_anchor().await?;
        let tx = node
            .post_data_tx(anchor, vec![50 + i; 32 + i as usize], &data_signer)
            .await;
        node.wait_for_mempool(tx.header.id, 10).await?;
        expected_data.insert(tx.header.id);

        let stake = node
            .post_stake_commitment_with_signer(&stake_signers[i as usize])
            .await?;
        node.wait_for_mempool_commitment_txs(vec![stake.id()], 10)
            .await?;
        expected_commit.insert(stake.id());
    }
    // Extra data so data list is longer than commitment (lists exhaust at different rates).
    for i in 0..3_u8 {
        let anchor = node.get_anchor().await?;
        let tx = node
            .post_data_tx(anchor, vec![200 + i; 48 + i as usize], &data_signer)
            .await;
        node.wait_for_mempool(tx.header.id, 10).await?;
        expected_data.insert(tx.header.id);
    }

    let (_, full) = fetch_mempool_txs(&client, &base, Some(500), None).await?;
    let full_data: BTreeSet<_> = full.data_txs.iter().map(|t| t.id).collect();
    let full_commit: BTreeSet<_> = full.commitment_txs.iter().map(|t| t.id).collect();
    assert_eq!(full_data, expected_data);
    assert_eq!(full_commit, expected_commit);
    assert!(full_data.len() >= 6);
    assert_eq!(full_commit.len(), 3);

    // limit=1 forces many pages; dual cursor must still cover both lists fully.
    let (paged_data, paged_commit) = collect_all_pages(&client, &base, 1).await?;
    assert_eq!(
        paged_data.into_iter().collect::<BTreeSet<_>>(),
        full_data,
        "mixed paging must not omit/dup data txs"
    );
    assert_eq!(
        paged_commit.into_iter().collect::<BTreeSet<_>>(),
        full_commit,
        "mixed paging must not omit/dup commitment txs"
    );

    node.stop().await;
    Ok(())
}
