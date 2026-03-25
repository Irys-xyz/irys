use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use crate::binary::ResolvedBinary;
use crate::cluster::{ClusterSpec, NodeSpec};
use crate::config::NodeRole;

pub(super) const HEIGHT_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared run directory for all tests in this invocation.
/// Set via `IRYS_RUN_ID` env var (xtask sets this to a timestamp).
/// Falls back to a timestamp generated at process start for standalone runs.
static RUN_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let run_id = std::env::var("IRYS_RUN_ID").unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        format!("{nanos}-{}", std::process::id())
    });
    let dir = repo_root()
        .join("target/multiversion/test-data")
        .join(&run_id);
    eprintln!("test run directory: {}", dir.display());
    dir
});

pub(super) const BASE_CONFIG: &str = include_str!("../../fixtures/base-config.toml");

const MINING_KEYS: &[&str] = &[
    "aaaa000000000000000000000000000000000000000000000000000000000001",
    "bbbb000000000000000000000000000000000000000000000000000000000002",
    "cccc000000000000000000000000000000000000000000000000000000000003",
];

const REWARD_ADDRESSES: &[&str] = &[
    "0x0000000000000000000000000000000000000001",
    "0x0000000000000000000000000000000000000002",
    "0x0000000000000000000000000000000000000003",
];

pub(super) fn repo_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(3)
        .expect("multiversion-tests should be at crates/tooling/multiversion-tests")
        .to_path_buf()
}

pub(super) fn genesis_spec(name: &str, binary: &ResolvedBinary, peers: Vec<String>) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        binary: binary.clone(),
        role: NodeRole::Genesis,
        peers,
        mining_key: MINING_KEYS[0].to_owned(),
        reward_address: REWARD_ADDRESSES[0].to_owned(),
    }
}

pub(super) fn peer_spec(
    name: &str,
    binary: &ResolvedBinary,
    index: usize,
    peers: Vec<String>,
) -> NodeSpec {
    // Offset by 1: index 0 in MINING_KEYS/REWARD_ADDRESSES is reserved for genesis.
    let key_index = index + 1;
    assert!(
        key_index < MINING_KEYS.len(),
        "peer_spec: index {index} out of range (max peer index {})",
        MINING_KEYS.len() - 2
    );
    assert!(
        key_index < REWARD_ADDRESSES.len(),
        "peer_spec: index {index} out of range (max peer index {})",
        REWARD_ADDRESSES.len() - 2
    );
    NodeSpec {
        name: name.to_owned(),
        binary: binary.clone(),
        role: NodeRole::Peer,
        peers,
        mining_key: MINING_KEYS[key_index].to_owned(),
        reward_address: REWARD_ADDRESSES[key_index].to_owned(),
    }
}

/// Asserts that a node is running the expected binary and responds to `/v1/info`.
pub(super) async fn assert_node_running_binary(
    cluster: &mut crate::cluster::Cluster,
    node_name: &str,
    expected_binary_path: &Path,
) {
    let node = cluster
        .nodes
        .get_mut(node_name)
        .unwrap_or_else(|| panic!("{node_name} missing from cluster"));
    assert!(node.is_running(), "{node_name} should be running");
    let api_url = node.api_url();
    let actual_binary = node
        .runtime_binary_path()
        .unwrap_or_else(|e| panic!("{node_name}: failed to read runtime binary: {e}"));
    let expected = std::fs::canonicalize(expected_binary_path)
        .unwrap_or_else(|_| expected_binary_path.to_path_buf());
    assert_eq!(
        actual_binary, expected,
        "{node_name} should be running the expected binary"
    );
    cluster
        .probe
        .get_info(&api_url)
        .await
        .unwrap_or_else(|e| panic!("{node_name} should respond to /v1/info: {e}"));
}

pub(super) fn cluster_spec(test_name: &str, nodes: Vec<NodeSpec>) -> ClusterSpec {
    cluster_spec_with_refs(test_name, nodes, None, None)
}

pub(super) fn cluster_spec_with_refs(
    test_name: &str,
    nodes: Vec<NodeSpec>,
    old_ref: Option<String>,
    new_ref: Option<String>,
) -> ClusterSpec {
    let run_dir = RUN_DIR.join(test_name);
    ClusterSpec {
        nodes,
        height_tolerance: 2,
        base_config: BASE_CONFIG.to_owned(),
        run_dir,
        old_ref,
        new_ref,
    }
}
