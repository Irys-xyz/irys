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

    // Functional smoke check: prove the chain accepts a data tx, uploads
    // the chunks, promotes it from Submit to Publish, and serves it back.
    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let tx = cluster
        .submit_promote_and_verify(
            "genesis",
            b"single_genesis_produces_blocks payload".to_vec(),
            chain_params.chain_id,
            chain_params.chunk_size,
            common::HEIGHT_TIMEOUT,
        )
        .await
        .expect("data tx did not promote on genesis");
    cluster
        .assert_tx_promoted_on_all_nodes(tx.header.id, common::HEIGHT_TIMEOUT)
        .await
        .expect("tx promoted on genesis but not on every node");

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

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    let tx = cluster
        .submit_promote_and_verify(
            "peer-1",
            b"same_version_two_node_convergence payload".to_vec(),
            chain_params.chain_id,
            chain_params.chunk_size,
            common::CONVERGENCE_TIMEOUT,
        )
        .await
        .expect("data tx submitted to peer-1 did not promote");
    cluster
        .assert_tx_promoted_on_all_nodes(tx.header.id, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("tx promoted on genesis but not on every node");

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

    let chain_params = cluster
        .fetch_chain_params()
        .await
        .expect("failed to fetch chain params");
    // Submit + promote to one peer, then a second tx through a different
    // node — proves gossip moves data + chunks both ways across the
    // 3-node fan-out and that the cluster can promote in either flow.
    let tx_a = cluster
        .submit_promote_and_verify(
            "peer-1",
            b"three_node_cluster_convergence peer-1 payload".to_vec(),
            chain_params.chain_id,
            chain_params.chunk_size,
            common::CONVERGENCE_TIMEOUT,
        )
        .await
        .expect("first data tx (via peer-1) did not promote");
    let tx_b = cluster
        .submit_promote_and_verify(
            "peer-2",
            b"three_node_cluster_convergence peer-2 payload".to_vec(),
            chain_params.chain_id,
            chain_params.chunk_size,
            common::CONVERGENCE_TIMEOUT,
        )
        .await
        .expect("second data tx (via peer-2) did not promote");
    cluster
        .assert_tx_promoted_on_all_nodes(tx_a.header.id, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("tx_a promoted on genesis but not on every node");
    cluster
        .assert_tx_promoted_on_all_nodes(tx_b.header.id, common::CONVERGENCE_TIMEOUT)
        .await
        .expect("tx_b promoted on genesis but not on every node");

    cluster.shutdown().await;
}
