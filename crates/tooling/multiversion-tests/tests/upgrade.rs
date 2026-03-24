mod common;

use irys_multiversion_tests::binary::{BinaryResolver, CURRENT_REF, ResolvedBinary};

async fn resolve_binaries() -> (ResolvedBinary, ResolvedBinary) {
    let old_ref = std::env::var("IRYS_OLD_REF").unwrap_or_else(|_| CURRENT_REF.to_owned());

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
    let mut cluster = irys_multiversion_tests::cluster::Cluster::start(spec)
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

    let upgraded = cluster
        .nodes
        .get_mut("peer-1")
        .expect("peer-1 missing after upgrade");
    assert!(upgraded.is_running());
    let api_url = upgraded.api_url();
    let actual_binary = upgraded
        .runtime_binary_path()
        .expect("should read running binary path");
    let expected_binary =
        std::fs::canonicalize(&new_binary.path).unwrap_or_else(|_| new_binary.path.clone());
    assert_eq!(
        actual_binary, expected_binary,
        "peer-1 should be running the new binary"
    );
    cluster
        .probe
        .get_info(&api_url)
        .await
        .expect("peer-1 should respond to /v1/info after upgrade");

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
    let mut cluster = irys_multiversion_tests::cluster::Cluster::start(spec)
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

    let expected_binary =
        std::fs::canonicalize(&new_binary.path).unwrap_or_else(|_| new_binary.path.clone());
    let mut api_urls = Vec::new();
    for (name, node) in &mut cluster.nodes {
        assert!(node.is_running(), "node {name} should be running");
        let actual = node
            .runtime_binary_path()
            .unwrap_or_else(|e| panic!("node {name}: failed to read runtime binary: {e}"));
        assert_eq!(
            actual, expected_binary,
            "node {name} should be running the new binary"
        );
        api_urls.push((name.clone(), node.api_url()));
    }
    for (name, url) in &api_urls {
        cluster
            .probe
            .get_info(url)
            .await
            .unwrap_or_else(|e| panic!("node {name} should respond to /v1/info: {e}"));
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
    let mut cluster = irys_multiversion_tests::cluster::Cluster::start(spec)
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

    let rolled_back = cluster
        .nodes
        .get_mut("peer-1")
        .expect("peer-1 missing after rollback");
    assert!(rolled_back.is_running());
    let api_url = rolled_back.api_url();
    let actual_binary = rolled_back
        .runtime_binary_path()
        .expect("should read running binary path");
    let expected_binary =
        std::fs::canonicalize(&old_binary.path).unwrap_or_else(|_| old_binary.path.clone());
    assert_eq!(
        actual_binary, expected_binary,
        "peer-1 should be running the old binary after rollback"
    );
    cluster
        .probe
        .get_info(&api_url)
        .await
        .expect("peer-1 should respond to /v1/info after rollback");

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
    let mut cluster = irys_multiversion_tests::cluster::Cluster::start(spec)
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

    let recovered = cluster
        .nodes
        .get_mut("peer-1")
        .expect("peer-1 missing after crash-upgrade");
    assert!(recovered.is_running());
    let api_url = recovered.api_url();
    let actual_binary = recovered
        .runtime_binary_path()
        .expect("should read running binary path");
    let expected_binary =
        std::fs::canonicalize(&new_binary.path).unwrap_or_else(|_| new_binary.path.clone());
    assert_eq!(
        actual_binary, expected_binary,
        "peer-1 should be running the new binary after crash-upgrade"
    );
    cluster
        .probe
        .get_info(&api_url)
        .await
        .expect("peer-1 should respond to /v1/info after crash-upgrade");

    cluster.shutdown().await;
}
