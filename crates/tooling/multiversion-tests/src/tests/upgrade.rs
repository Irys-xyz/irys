use super::helpers as common;
use crate::binary::{BinaryResolver, CURRENT_REF, ResolvedBinary};
use crate::cluster::Cluster;
use irys_types::DataTransaction;
use std::time::Duration;

/// Submits a data tx through `target_node` with `label` baked into the
/// payload, uploads its chunks, waits for promotion, then asserts every
/// running node serves the new tx *and* every previously submitted tx
/// via `/v1/tx/{id}` — and that every node still reports each tx as
/// promoted. This is the property we actually care about across upgrades
/// and rollbacks: a node must not silently lose historical data (header
/// content *or* promotion state) when its binary changes.
async fn submit_and_assert_full_history(
    cluster: &mut Cluster,
    target_node: &str,
    label: &str,
    chain_id: u64,
    chunk_size: u64,
    history: &mut Vec<DataTransaction>,
    timeout: Duration,
) {
    let payload = format!("multiversion-tests::{label}").into_bytes();
    let tx = cluster
        .submit_promote_and_verify(target_node, payload, chain_id, chunk_size, timeout)
        .await
        .unwrap_or_else(|e| panic!("submit/promote {label} via {target_node}: {e}"));
    history.push(tx);

    // Re-verify the entire tx history on every running node — proves no
    // node silently dropped or corrupted prior records when a peer's
    // binary swapped underneath the cluster.
    let urls = cluster
        .checked_api_urls()
        .unwrap_or_else(|e| panic!("nodes unhealthy after {label}: {e}"));
    let client = reqwest::Client::new();
    for tx in history.iter() {
        let tx_id = tx.header.id;
        for url in &urls {
            crate::data_tx::wait_for_tx_visible(&client, url, tx_id, timeout)
                .await
                .unwrap_or_else(|e| {
                    panic!("after {label}: tx {tx_id} not visible at {url}: {e}");
                });
        }
    }
}

/// Asserts every running node still reports promotion for every tx in
/// `history`. This is the rollback-safety check that catches the
/// `promoted_height` move from inline header to `IrysDataTxMetadata`
/// table: an OLD binary rolled back on top of a NEW-migrated DB doesn't
/// know about the new metadata table, and would silently report
/// `promotion_height: None` for previously-promoted txs.
async fn assert_history_promoted(
    cluster: &mut Cluster,
    label: &str,
    history: &[DataTransaction],
    timeout: Duration,
) {
    for tx in history {
        cluster
            .assert_tx_promoted_on_all_nodes(tx.header.id, timeout)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{label}: tx {} not reported as promoted on every node: {e}",
                    tx.header.id
                )
            });
    }
}

/// Strict counterpart of [`submit_and_assert_full_history`]'s history check:
/// instead of just confirming `/v1/tx/{id}` returns *some* 2xx body on every
/// node, this asserts each node returns the **full** signed header — every
/// shared field, byte-for-byte against what we originally submitted. This
/// is the post-rollback sanity check — it catches the failure mode where
/// an older binary mis-decodes records a newer binary wrote and silently
/// serves shifted bytes on a field we wouldn't otherwise have sampled.
async fn assert_full_history_matches(
    cluster: &mut Cluster,
    label: &str,
    history: &[DataTransaction],
) {
    for tx in history.iter() {
        cluster.assert_tx_matches_on_all_nodes(tx).await.unwrap_or_else(|e| {
            panic!(
                "{label}: tx {} returned content that does not match the original on at least one node: {e}",
                tx.header.id
            )
        });
    }
}

/// Asserts every running node serves byte-identical block headers for
/// every block the cluster has finalized. Catches a class of cross-
/// version drift that the tx-header check misses: a `BlockHeader`
/// carries enum-tagged sub-fields (PoA, ledger metadata, signatures)
/// whose `Compact`/JSON shape can drift independently of the tx-header
/// `Compact` layout. The cluster's running genesis serves as the
/// reference; every other node must agree.
async fn assert_block_headers_consistent_across_nodes(cluster: &mut Cluster) {
    cluster
        .assert_block_index_consistent()
        .await
        .unwrap_or_else(|e| panic!("block headers differ across nodes: {e}"));
}

async fn resolve_binaries() -> (ResolvedBinary, ResolvedBinary) {
    let old_ref = std::env::var("IRYS_OLD_REF").unwrap_or_else(|_| CURRENT_REF.to_owned());
    if old_ref == CURRENT_REF {
        panic!(
            "IRYS_OLD_REF is unset or resolves to {CURRENT_REF}; upgrade/rollback tests require \
             a different revision for the old binary (e.g. IRYS_OLD_REF=master)"
        );
    }

    let resolver = BinaryResolver::new(&common::repo_root());
    let old = resolver
        .resolve_old(&old_ref)
        .await
        .unwrap_or_else(|e| panic!("failed to build old binary from {old_ref}: {e}"));
    let new = resolver
        .resolve_new()
        .await
        .expect("failed to build new binary");
    (old, new)
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binaries from HEAD and master"]
async fn upgrade_one_node_in_running_cluster() {
    let (old_binary, new_binary) = resolve_binaries().await;

    let spec = common::cluster_spec_with_refs(
        "upgrade_one_node_in_running_cluster",
        vec![
            common::genesis_spec("genesis", &old_binary, vec![]),
            common::peer_spec("peer-1", &old_binary, 0, vec!["genesis".to_owned()]),
            common::peer_spec("peer-2", &old_binary, 1, vec!["genesis".to_owned()]),
        ],
        Some(old_binary.git_rev.clone()),
        Some(new_binary.git_rev.clone()),
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("initial cluster did not converge");

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let mut history: Vec<DataTransaction> = Vec::new();
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "upgrade_one_node:pre-upgrade",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    let baseline = cluster
        .get_max_height()
        .await
        .expect("failed to get baseline height before upgrade");

    cluster
        .upgrade_node("peer-1", &new_binary)
        .await
        .expect("failed to upgrade peer-1");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("cluster did not converge after upgrading peer-1");

    cluster
        .wait_for_height_above(baseline, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("chain did not advance after upgrading peer-1");

    common::assert_node_running_binary(&mut cluster, "peer-1", &new_binary.path).await;

    // Post-upgrade: re-verify the prior tx and submit a new one through the
    // upgraded peer-1, asserting NEW peer-1 can both serve OLD-written data
    // and write fresh data that propagates back to the still-OLD peers.
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "upgrade_one_node:post-upgrade",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;
    assert_history_promoted(
        &mut cluster,
        "upgrade_one_node:post-upgrade",
        &history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    cluster.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binaries from HEAD and master"]
async fn rolling_upgrade_all_nodes() {
    let (old_binary, new_binary) = resolve_binaries().await;

    let spec = common::cluster_spec_with_refs(
        "rolling_upgrade_all_nodes",
        vec![
            common::genesis_spec("genesis", &old_binary, vec![]),
            common::peer_spec("peer-1", &old_binary, 0, vec!["genesis".to_owned()]),
            common::peer_spec("peer-2", &old_binary, 1, vec!["genesis".to_owned()]),
        ],
        Some(old_binary.git_rev.clone()),
        Some(new_binary.git_rev.clone()),
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("initial cluster did not converge");

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let mut history: Vec<DataTransaction> = Vec::new();
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "rolling_upgrade:pre-upgrade",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    for name in ["peer-1", "peer-2", "genesis"] {
        let baseline = cluster
            .get_max_height()
            .await
            .unwrap_or_else(|e| panic!("failed to get baseline before upgrading {name}: {e}"));

        tracing::info!(node = %name, "upgrading node");
        cluster
            .upgrade_node(name, &new_binary)
            .await
            .unwrap_or_else(|e| panic!("failed to upgrade {name}: {e}"));

        cluster
            .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("cluster did not converge after upgrading {name}: {e}"));

        cluster
            .wait_for_height_above(baseline, common::CONVERGENCE_TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("chain did not advance after upgrading {name}: {e}"));

        // After every step in the rolling upgrade, submit through the node
        // we just upgraded — asserts the freshly-swapped binary can both
        // accept new traffic and serve every prior tx. We deliberately
        // don't run the promotion check here: the just-submitted tx may
        // still be racing to propagate to a peer whose
        // `block_migration_service` is catching up after the binary
        // swap (especially right after genesis is restarted, since it's
        // the only miner and the chain briefly stalls). The end-of-test
        // promotion check below covers the meaningful property
        // (long-lived txs must remain promoted across every transition)
        // without the timing flake.
        submit_and_assert_full_history(
            &mut cluster,
            name,
            &format!("rolling_upgrade:after-{name}"),
            chain_params.chain_id,
            chain_params.chunk_size,
            &mut history,
            common::CONVERGENCE_TIMEOUT,
        )
        .await;
    }

    let node_names: Vec<String> = cluster.nodes.keys().cloned().collect();
    for name in &node_names {
        common::assert_node_running_binary(&mut cluster, name, &new_binary.path).await;
    }

    // Final promotion check, run once after every node has rolled to NEW.
    // By now the chain has had time to advance past the migration depth
    // for every tx submitted along the way, including the last one
    // submitted right after the genesis upgrade — so every node should
    // have all of `history` recorded as promoted in its
    // `IrysDataTxMetadata` table. If any node lost promotion state
    // across one of the upgrades (the failure mode we care about), it
    // surfaces here.
    assert_history_promoted(
        &mut cluster,
        "rolling_upgrade:final",
        &history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    cluster.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binaries from HEAD and master"]
async fn rollback_after_upgrade() {
    let (old_binary, new_binary) = resolve_binaries().await;

    let spec = common::cluster_spec_with_refs(
        "rollback_after_upgrade",
        vec![
            common::genesis_spec("genesis", &old_binary, vec![]),
            common::peer_spec("peer-1", &old_binary, 0, vec!["genesis".to_owned()]),
        ],
        Some(old_binary.git_rev.clone()),
        Some(new_binary.git_rev.clone()),
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("initial cluster did not converge");

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let mut history: Vec<DataTransaction> = Vec::new();
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "rollback:pre-upgrade",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    // The pre-upgrade tx only lands in the in-memory block tree until the
    // chain advances past `block_migration_depth` (default 6) — only then
    // does it get persisted to the on-disk `IrysDataTxHeaders` table that
    // V1→V2 migration actually rewrites. Without this wait, the migration
    // runs on an empty table and the rollback test exercises nothing
    // interesting. We pad the depth by 4 blocks for safety.
    let pre_upgrade_height = cluster
        .get_max_height()
        .await
        .expect("failed to read pre-upgrade height");
    cluster
        .wait_for_height_at_least(
            pre_upgrade_height + chain_params.block_migration_depth + 4,
            common::HEIGHT_TIMEOUT,
        )
        .await
        .expect("chain did not advance past block_migration_depth before upgrade");

    let baseline = cluster
        .get_max_height()
        .await
        .expect("failed to get baseline height before upgrade");

    cluster
        .upgrade_node("peer-1", &new_binary)
        .await
        .expect("failed to upgrade peer-1");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("cluster did not converge after upgrade");

    cluster
        .wait_for_height_above(baseline, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("chain did not advance after upgrading peer-1");

    // Populate aggressively under NEW. Each submitted tx must again age
    // past `block_migration_depth` so it lands in peer-1's persistent DB
    // in NEW format before we yank the binary back to OLD.
    for round in 0..3 {
        submit_and_assert_full_history(
            &mut cluster,
            "peer-1",
            &format!("rollback:post-upgrade-{round}"),
            chain_params.chain_id,
            chain_params.chunk_size,
            &mut history,
            common::CONVERGENCE_TIMEOUT,
        )
        .await;
    }
    let post_upgrade_height = cluster
        .get_max_height()
        .await
        .expect("failed to read post-upgrade height");
    cluster
        .wait_for_height_at_least(
            post_upgrade_height + chain_params.block_migration_depth + 4,
            common::HEIGHT_TIMEOUT,
        )
        .await
        .expect("chain did not advance enough blocks under NEW for migration writes to commit");

    let baseline = cluster
        .get_max_height()
        .await
        .expect("failed to get baseline height before rollback");

    cluster
        .upgrade_node("peer-1", &old_binary)
        .await
        .expect("failed to rollback peer-1");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("cluster did not converge after rollback");

    cluster
        .wait_for_height_above(baseline, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("chain did not advance after rollback");

    common::assert_node_running_binary(&mut cluster, "peer-1", &old_binary.path).await;

    // Strict full-header check: every signed field of every historical tx
    // must read back identically on every node, including the rolled-back
    // peer-1 which is now decoding V2/V3-encoded `IrysDataTxHeaders`
    // records through OLD's V1 `Compact` decoder. Any byte-layout drift
    // across the migration that affects fields the V1 schema knows about
    // would surface here.
    assert_full_history_matches(&mut cluster, "rollback:post-rollback (strict)", &history).await;

    // Block-header consistency: every node must agree on the block that
    // contains each tx, field-for-field. This catches a different class
    // of cross-version bug than the tx-header check — a block header
    // carries enum-tagged fields (PoA, signatures, ledger metadata) that
    // could drift independently of the tx-header `Compact` layout.
    assert_block_headers_consistent_across_nodes(&mut cluster).await;

    // Promotion-state preservation: V1→V2 migration moves
    // `promoted_height` out of the inline `IrysDataTxHeaders` table and
    // into a separate `IrysDataTxMetadata` table. An OLD binary rolled
    // back on top of that schema doesn't know to look in the new table,
    // so it would silently report `promotion_height: None` for every
    // previously-promoted tx — losing the chain's "this data is in
    // permanent storage" signal. This check fires the alarm.
    assert_history_promoted(
        &mut cluster,
        "rollback:post-rollback",
        &history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    // Fresh writes after rollback must propagate end-to-end and pass the
    // same strict checks. This is what proves OLD didn't just keep
    // serving old data — it can still mine, gossip, serialize, and
    // promote.
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "rollback:post-rollback",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;
    assert_full_history_matches(
        &mut cluster,
        "rollback:post-rollback-write (strict)",
        &history,
    )
    .await;
    assert_block_headers_consistent_across_nodes(&mut cluster).await;
    assert_history_promoted(
        &mut cluster,
        "rollback:post-rollback-write",
        &history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    cluster.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binaries from HEAD and master"]
async fn crash_during_upgrade() {
    let (old_binary, new_binary) = resolve_binaries().await;

    let spec = common::cluster_spec_with_refs(
        "crash_during_upgrade",
        vec![
            common::genesis_spec("genesis", &old_binary, vec![]),
            common::peer_spec("peer-1", &old_binary, 0, vec!["genesis".to_owned()]),
        ],
        Some(old_binary.git_rev.clone()),
        Some(new_binary.git_rev.clone()),
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("initial cluster did not converge");

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let mut history: Vec<DataTransaction> = Vec::new();
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "crash_during_upgrade:pre-crash",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    let baseline = cluster
        .get_max_height()
        .await
        .expect("failed to get baseline height before crash-upgrade");

    cluster
        .node_mut("peer-1")
        .expect("peer-1 not found")
        .kill()
        .await
        .expect("failed to kill peer-1");

    cluster
        .upgrade_node("peer-1", &new_binary)
        .await
        .expect("failed to upgrade crashed peer-1");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("cluster did not converge after crash-upgrade");

    cluster
        .wait_for_height_above(baseline, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("chain did not advance after crash-upgrade");

    common::assert_node_running_binary(&mut cluster, "peer-1", &new_binary.path).await;

    // The crashed-then-upgraded peer must come back with its prior tx
    // history intact and able to accept new writes.
    submit_and_assert_full_history(
        &mut cluster,
        "peer-1",
        "crash_during_upgrade:post-recovery",
        chain_params.chain_id,
        chain_params.chunk_size,
        &mut history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;
    assert_history_promoted(
        &mut cluster,
        "crash_during_upgrade:post-recovery",
        &history,
        common::CONVERGENCE_TIMEOUT,
    )
    .await;

    cluster.shutdown().await;
}
