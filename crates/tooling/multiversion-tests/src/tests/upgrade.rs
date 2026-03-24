use super::helpers as common;
use crate::binary::{BinaryResolver, CURRENT_REF, ResolvedBinary};

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
    }

    let node_names: Vec<String> = cluster.nodes.keys().cloned().collect();
    for name in &node_names {
        common::assert_node_running_binary(&mut cluster, name, &new_binary.path).await;
    }

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
        .expect("chain did not advance after upgrade");

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

    cluster.shutdown().await;
}
