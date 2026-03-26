use super::helpers as common;
use crate::binary::BinaryResolver;

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binary from HEAD"]
async fn single_genesis_produces_blocks() {
    let resolver = BinaryResolver::new(&common::repo_root());
    let binary = resolver
        .resolve_new()
        .await
        .expect("failed to build binary from HEAD");

    let spec = common::cluster_spec(
        "single_genesis_produces_blocks",
        vec![common::genesis_spec("genesis", &binary, vec![])],
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    let genesis_url = cluster
        .nodes
        .get_mut("genesis")
        .expect("genesis node missing")
        .api_url();

    cluster
        .probe
        .wait_for_height(&genesis_url, 3, common::HEIGHT_TIMEOUT)
        .await
        .expect("genesis did not reach height 3");

    cluster.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binary from HEAD"]
async fn same_version_two_node_convergence() {
    let resolver = BinaryResolver::new(&common::repo_root());
    let binary = resolver
        .resolve_new()
        .await
        .expect("failed to build binary from HEAD");

    let spec = common::cluster_spec(
        "same_version_two_node_convergence",
        vec![
            common::genesis_spec("genesis", &binary, vec![]),
            common::peer_spec("peer-1", &binary, 0, vec!["genesis".to_owned()]),
        ],
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("two-node cluster did not converge");

    let urls = cluster
        .checked_api_urls()
        .expect("all nodes should be running");
    for url in &urls {
        let info = cluster
            .probe
            .get_info(url)
            .await
            .expect("failed to query node info");
        assert!(info.height >= 1, "node at {url} did not produce blocks");
    }

    cluster.shutdown().await;
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binary from HEAD"]
async fn three_node_cluster_convergence() {
    let resolver = BinaryResolver::new(&common::repo_root());
    let binary = resolver
        .resolve_new()
        .await
        .expect("failed to build binary from HEAD");

    let spec = common::cluster_spec(
        "three_node_cluster_convergence",
        vec![
            common::genesis_spec("genesis", &binary, vec![]),
            common::peer_spec("peer-1", &binary, 0, vec!["genesis".to_owned()]),
            common::peer_spec("peer-2", &binary, 1, vec!["genesis".to_owned()]),
        ],
    );
    let mut cluster = crate::cluster::Cluster::start(spec)
        .await
        .expect("failed to start cluster");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("three-node cluster did not converge");

    let urls = cluster
        .checked_api_urls()
        .expect("all nodes should be running");
    assert_eq!(urls.len(), 3, "expected 3 running nodes");

    cluster.shutdown().await;
}
