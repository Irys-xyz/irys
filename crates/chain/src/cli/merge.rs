use std::path::PathBuf;
use std::time::Duration;

use eyre::{bail, WrapErr as _};
use irys_types::{ConsensusOptions, NodeConfig, NodeMode, PeerFilterMode, SyncMode};

use super::NodeCommand;

/// Apply CLI argument overrides to a loaded NodeConfig.
///
/// For each `Some(val)` in the CLI args, the corresponding config field is overwritten.
/// `None` values (flags not provided) leave the config unchanged.
pub fn apply_cli_overrides(mut config: NodeConfig, cmd: &NodeCommand) -> eyre::Result<NodeConfig> {
    // -- Node args --
    if let Some(ref dir) = cmd.node.base_directory {
        config.base_directory = dir.clone();
    }

    if let Some(ref mode) = cmd.node.node_mode {
        config.node_mode = parse_node_mode(mode)?;
    }

    if let Some(ref mode) = cmd.node.sync_mode {
        config.sync_mode = parse_sync_mode(mode)?;
    }

    if let Some(ref consensus) = cmd.node.consensus {
        config.consensus = parse_consensus(consensus);
    }

    if let Some(ref addr) = cmd.node.reward_address {
        config.reward_address = addr
            .parse()
            .map_err(|e: String| eyre::eyre!("invalid reward address {addr:?}: {e}"))?;
    }

    if let Some(v) = cmd.node.stake_pledge_drives {
        config.stake_pledge_drives = v;
    }

    if let Some(v) = cmd.node.genesis_peer_discovery_timeout_millis {
        config.genesis_peer_discovery_timeout_millis = v;
    }

    // -- Genesis flag (overrides node_mode) --
    if cmd.genesis {
        config.node_mode = NodeMode::Genesis;
    }

    // -- MINING_KEY env var --
    if let Ok(key_hex) = std::env::var("MINING_KEY") {
        let key_bytes =
            hex::decode(key_hex.trim()).wrap_err("MINING_KEY env var is not valid hex")?;
        let signing_key = k256::ecdsa::SigningKey::from_slice(&key_bytes)
            .wrap_err("MINING_KEY env var is not a valid secp256k1 private key")?;
        config.mining_key = signing_key;
    }

    // -- Network defaults --
    if let Some(ref ip) = cmd.network.public_ip {
        config.network_defaults.public_ip = ip.clone();
    }
    if let Some(ref ip) = cmd.network.bind_ip {
        config.network_defaults.bind_ip = ip.clone();
    }

    // -- HTTP --
    if let Some(port) = cmd.http.port {
        config.http.bind_port = port;
    }
    if let Some(ref ip) = cmd.http.bind_ip {
        config.http.bind_ip = Some(ip.clone());
    }
    if let Some(port) = cmd.http.public_port {
        config.http.public_port = port;
    }

    // -- Gossip --
    if let Some(port) = cmd.gossip.port {
        config.gossip.bind_port = port;
    }
    if let Some(ref ip) = cmd.gossip.bind_ip {
        config.gossip.bind_ip = Some(ip.clone());
    }
    if let Some(port) = cmd.gossip.public_port {
        config.gossip.public_port = port;
    }

    // -- Reth --
    if let Some(port) = cmd.reth.port {
        config.reth.network.bind_port = port;
    }
    if let Some(ref ip) = cmd.reth.bind_ip {
        config.reth.network.bind_ip = Some(ip.clone());
    }
    if let Some(port) = cmd.reth.public_port {
        config.reth.network.public_port = port;
    }

    // -- Peer filter --
    if let Some(ref mode) = cmd.peer.peer_filter_mode {
        config.peer_filter_mode = parse_peer_filter_mode(mode)?;
    }

    // -- Storage sync --
    if let Some(v) = cmd.storage.num_writes_before_sync {
        config.storage.num_writes_before_sync = v;
    }

    // -- Data sync --
    if let Some(v) = cmd.storage.max_pending_chunk_requests {
        config.data_sync.max_pending_chunk_requests = v;
    }
    if let Some(v) = cmd.storage.max_storage_throughput_bps {
        config.data_sync.max_storage_throughput_bps = v;
    }
    if let Some(secs) = cmd.storage.bandwidth_adjustment_interval_secs {
        config.data_sync.bandwidth_adjustment_interval = Duration::from_secs(secs);
    }
    if let Some(secs) = cmd.storage.chunk_request_timeout_secs {
        config.data_sync.chunk_request_timeout = Duration::from_secs(secs);
    }

    // -- Packing --
    if let Some(v) = cmd.packing.cpu_packing_concurrency {
        config.packing.local.cpu_packing_concurrency = v;
    }
    if let Some(v) = cmd.packing.gpu_packing_batch_size {
        config.packing.local.gpu_packing_batch_size = v;
    }

    // -- Cache --
    if let Some(v) = cmd.cache.cache_clean_lag {
        config.cache.cache_clean_lag = v;
    }
    if let Some(v) = cmd.cache.max_cache_size_bytes {
        config.cache.max_cache_size_bytes = v;
    }
    if let Some(v) = cmd.cache.prune_at_capacity_percent {
        config.cache.prune_at_capacity_percent = v;
    }

    // -- VDF --
    if let Some(v) = cmd.vdf.parallel_verification_thread_limit {
        config.vdf.parallel_verification_thread_limit = v;
    }

    // -- Mempool --
    if let Some(v) = cmd.mempool.max_pending_pledge_items {
        config.mempool.max_pending_pledge_items = v;
    }
    if let Some(v) = cmd.mempool.max_pledges_per_item {
        config.mempool.max_pledges_per_item = v;
    }
    if let Some(v) = cmd.mempool.max_pending_chunk_items {
        config.mempool.max_pending_chunk_items = v;
    }
    if let Some(v) = cmd.mempool.max_chunks_per_item {
        config.mempool.max_chunks_per_item = v;
    }
    if let Some(v) = cmd.mempool.max_preheader_chunks_per_item {
        config.mempool.max_preheader_chunks_per_item = v;
    }
    if let Some(v) = cmd.mempool.max_preheader_data_path_bytes {
        config.mempool.max_preheader_data_path_bytes = v;
    }
    if let Some(v) = cmd.mempool.max_valid_items {
        config.mempool.max_valid_items = v;
    }
    if let Some(v) = cmd.mempool.max_invalid_items {
        config.mempool.max_invalid_items = v;
    }
    if let Some(v) = cmd.mempool.max_valid_chunks {
        config.mempool.max_valid_chunks = v;
    }
    if let Some(v) = cmd.mempool.max_valid_submit_txs {
        config.mempool.max_valid_submit_txs = v;
    }
    if let Some(v) = cmd.mempool.max_valid_commitment_addresses {
        config.mempool.max_valid_commitment_addresses = v;
    }
    if let Some(v) = cmd.mempool.max_commitments_per_address {
        config.mempool.max_commitments_per_address = v;
    }
    if let Some(v) = cmd.mempool.max_concurrent_mempool_tasks {
        config.mempool.max_concurrent_mempool_tasks = v;
    }
    if let Some(v) = cmd.mempool.chunk_writer_buffer_size {
        config.mempool.chunk_writer_buffer_size = v;
    }

    // -- P2P handshake --
    if let Some(v) = cmd.p2p.max_concurrent_handshakes {
        config.p2p_handshake.max_concurrent_handshakes = v;
    }
    if let Some(v) = cmd.p2p.max_peers_per_response {
        config.p2p_handshake.max_peers_per_response = v;
    }
    if let Some(v) = cmd.p2p.max_retries {
        config.p2p_handshake.max_retries = v;
    }
    if let Some(v) = cmd.p2p.backoff_base_secs {
        config.p2p_handshake.backoff_base_secs = v;
    }
    if let Some(v) = cmd.p2p.backoff_cap_secs {
        config.p2p_handshake.backoff_cap_secs = v;
    }
    if let Some(v) = cmd.p2p.blocklist_ttl_secs {
        config.p2p_handshake.blocklist_ttl_secs = v;
    }
    if let Some(v) = cmd.p2p.server_peer_list_cap {
        config.p2p_handshake.server_peer_list_cap = v;
    }

    // -- P2P gossip --
    if let Some(v) = cmd.p2p.broadcast_batch_size {
        config.p2p_gossip.broadcast_batch_size = v;
    }
    if let Some(v) = cmd.p2p.broadcast_batch_throttle_interval {
        config.p2p_gossip.broadcast_batch_throttle_interval = v;
    }
    if let Some(v) = cmd.p2p.enable_scoring {
        config.p2p_gossip.enable_scoring = v;
    }
    if let Some(v) = cmd.p2p.max_concurrent_gossip_chunks {
        config.p2p_gossip.max_concurrent_gossip_chunks = v;
    }

    // -- P2P pull --
    if let Some(v) = cmd.p2p.top_active_window {
        config.p2p_pull.top_active_window = v;
    }
    if let Some(v) = cmd.p2p.sample_size {
        config.p2p_pull.sample_size = v;
    }
    if let Some(v) = cmd.p2p.max_attempts {
        config.p2p_pull.max_attempts = v;
    }

    // -- Sync --
    if let Some(v) = cmd.sync.block_batch_size {
        config.sync.block_batch_size = v;
    }
    if let Some(v) = cmd.sync.periodic_sync_check_interval_secs {
        config.sync.periodic_sync_check_interval_secs = v;
    }
    if let Some(v) = cmd.sync.retry_block_request_timeout_secs {
        config.sync.retry_block_request_timeout_secs = v;
    }
    if let Some(v) = cmd.sync.enable_periodic_sync_check {
        config.sync.enable_periodic_sync_check = v;
    }
    if let Some(v) = cmd.sync.wait_queue_slot_timeout_secs {
        config.sync.wait_queue_slot_timeout_secs = v;
    }
    if let Some(v) = cmd.sync.wait_queue_slot_max_attempts {
        config.sync.wait_queue_slot_max_attempts = v;
    }

    Ok(config)
}

fn parse_node_mode(s: &str) -> eyre::Result<NodeMode> {
    match s.to_lowercase().as_str() {
        "genesis" => Ok(NodeMode::Genesis),
        "peer" => Ok(NodeMode::Peer),
        _ => bail!("invalid node mode: {s:?} (expected \"genesis\" or \"peer\")"),
    }
}

fn parse_sync_mode(s: &str) -> eyre::Result<SyncMode> {
    match s.to_lowercase().as_str() {
        "trusted" => Ok(SyncMode::Trusted),
        "full" => Ok(SyncMode::Full),
        _ => bail!("invalid sync mode: {s:?} (expected \"trusted\" or \"full\")"),
    }
}

fn parse_consensus(s: &str) -> ConsensusOptions {
    match s.to_lowercase().as_str() {
        "testing" => ConsensusOptions::Testing,
        "testnet" => ConsensusOptions::Testnet,
        "mainnet" => ConsensusOptions::Mainnet,
        _ => ConsensusOptions::Path(PathBuf::from(s)),
    }
}

fn parse_peer_filter_mode(s: &str) -> eyre::Result<PeerFilterMode> {
    match s.to_lowercase().replace('_', "-").as_str() {
        "unrestricted" => Ok(PeerFilterMode::Unrestricted),
        "trusted-only" => Ok(PeerFilterMode::TrustedOnly),
        "trusted-and-handshake" => Ok(PeerFilterMode::TrustedAndHandshake),
        _ => bail!(
            "invalid peer filter mode: {s:?} (expected \"unrestricted\", \"trusted-only\", or \"trusted-and-handshake\")"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::IrysCli;
    use clap::Parser as _;

    fn test_config() -> NodeConfig {
        NodeConfig::testing()
    }

    fn parse_node_cmd(args: &[&str]) -> NodeCommand {
        let mut full_args = vec!["irys", "node"];
        full_args.extend_from_slice(args);
        let cli = IrysCli::try_parse_from(full_args).expect("failed to parse CLI args");
        match cli.command {
            Some(super::super::Commands::Node(cmd)) => *cmd,
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_no_overrides_preserves_config() {
        let config = test_config();
        let original_port = config.http.bind_port;
        let cmd = parse_node_cmd(&[]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.http.bind_port, original_port);
    }

    #[test]
    fn test_http_port_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--http.port", "9080"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.http.bind_port, 9080);
    }

    #[test]
    fn test_gossip_port_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--gossip.port", "4444"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.gossip.bind_port, 4444);
    }

    #[test]
    fn test_reth_port_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--reth.port", "30304"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.reth.network.bind_port, 30304);
    }

    #[test]
    fn test_genesis_flag() {
        let mut config = test_config();
        config.node_mode = NodeMode::Peer;
        let cmd = parse_node_cmd(&["--genesis"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Genesis);
    }

    #[test]
    fn test_node_mode_override() {
        let mut config = test_config();
        config.node_mode = NodeMode::Genesis;
        let cmd = parse_node_cmd(&["--node-mode", "peer"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Peer);
    }

    #[test]
    fn test_sync_mode_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--sync-mode", "full"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.sync_mode, SyncMode::Full);
    }

    #[test]
    fn test_consensus_override_testnet() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--consensus", "testnet"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert!(matches!(merged.consensus, ConsensusOptions::Testnet));
    }

    #[test]
    fn test_consensus_override_path() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--consensus", "/custom/path.toml"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert!(
            matches!(merged.consensus, ConsensusOptions::Path(p) if p == std::path::Path::new("/custom/path.toml"))
        );
    }

    #[test]
    fn test_peer_filter_mode_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--peer-filter-mode", "trusted-only"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.peer_filter_mode, PeerFilterMode::TrustedOnly);
    }

    #[test]
    fn test_peer_filter_mode_accepts_underscores() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--peer-filter-mode", "trusted_and_handshake"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.peer_filter_mode, PeerFilterMode::TrustedAndHandshake);
    }

    #[test]
    fn test_network_defaults_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--network.public-ip",
            "1.2.3.4",
            "--network.bind-ip",
            "0.0.0.0",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.network_defaults.public_ip, "1.2.3.4");
        assert_eq!(merged.network_defaults.bind_ip, "0.0.0.0");
    }

    #[test]
    fn test_bind_ip_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--http.bind-ip",
            "10.0.0.1",
            "--gossip.bind-ip",
            "10.0.0.2",
            "--reth.bind-ip",
            "10.0.0.3",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.http.bind_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(merged.gossip.bind_ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(merged.reth.network.bind_ip.as_deref(), Some("10.0.0.3"));
    }

    #[test]
    fn test_public_port_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--http.public-port",
            "8080",
            "--gossip.public-port",
            "4000",
            "--reth.public-port",
            "30000",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.http.public_port, 8080);
        assert_eq!(merged.gossip.public_port, 4000);
        assert_eq!(merged.reth.network.public_port, 30000);
    }

    #[test]
    fn test_datadir_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--datadir", "/tmp/irys-data"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.base_directory, PathBuf::from("/tmp/irys-data"));
    }

    #[test]
    fn test_invalid_node_mode() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--node-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd).is_err());
    }

    #[test]
    fn test_invalid_sync_mode() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--sync-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd).is_err());
    }

    #[test]
    fn test_invalid_peer_filter_mode() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--peer-filter-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd).is_err());
    }

    #[test]
    fn test_multiple_overrides_combined() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--http.port",
            "9090",
            "--gossip.port",
            "5555",
            "--node-mode",
            "genesis",
            "--sync-mode",
            "full",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.http.bind_port, 9090);
        assert_eq!(merged.gossip.bind_port, 5555);
        assert_eq!(merged.node_mode, NodeMode::Genesis);
        assert_eq!(merged.sync_mode, SyncMode::Full);
    }

    #[test]
    fn test_genesis_flag_overrides_node_mode_arg() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--node-mode", "peer", "--genesis"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Genesis);
    }

    // -- Storage & data sync tests --

    #[test]
    fn test_storage_sync_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--storage.num-writes-before-sync", "500"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.storage.num_writes_before_sync, 500);
    }

    #[test]
    fn test_data_sync_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--storage.max-pending-chunk-requests",
            "2000",
            "--storage.max-throughput-bps",
            "104857600",
            "--storage.bandwidth-adjust-interval",
            "10",
            "--storage.chunk-request-timeout",
            "30",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.data_sync.max_pending_chunk_requests, 2000);
        assert_eq!(merged.data_sync.max_storage_throughput_bps, 104857600);
        assert_eq!(
            merged.data_sync.bandwidth_adjustment_interval,
            Duration::from_secs(10)
        );
        assert_eq!(
            merged.data_sync.chunk_request_timeout,
            Duration::from_secs(30)
        );
    }

    // -- Packing tests --

    #[test]
    fn test_packing_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--packing.cpu-concurrency",
            "8",
            "--packing.gpu-batch-size",
            "256",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.packing.local.cpu_packing_concurrency, 8);
        assert_eq!(merged.packing.local.gpu_packing_batch_size, 256);
    }

    // -- Cache tests --

    #[test]
    fn test_cache_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--cache.clean-lag",
            "5",
            "--cache.max-size-bytes",
            "5368709120",
            "--cache.prune-at-percent",
            "90.5",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.cache.cache_clean_lag, 5);
        assert_eq!(merged.cache.max_cache_size_bytes, 5368709120);
        assert!((merged.cache.prune_at_capacity_percent - 90.5).abs() < f64::EPSILON);
    }

    // -- VDF tests --

    #[test]
    fn test_vdf_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--vdf.parallel-threads", "8"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.vdf.parallel_verification_thread_limit, 8);
    }

    // -- Mempool tests --

    #[test]
    fn test_mempool_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--mempool.max-pending-pledge-items",
            "2000",
            "--mempool.max-pledges-per-item",
            "20",
            "--mempool.max-valid-items",
            "50000",
            "--mempool.max-concurrent-tasks",
            "60",
            "--mempool.chunk-writer-buffer-size",
            "8192",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.mempool.max_pending_pledge_items, 2000);
        assert_eq!(merged.mempool.max_pledges_per_item, 20);
        assert_eq!(merged.mempool.max_valid_items, 50000);
        assert_eq!(merged.mempool.max_concurrent_mempool_tasks, 60);
        assert_eq!(merged.mempool.chunk_writer_buffer_size, 8192);
    }

    #[test]
    fn test_mempool_chunk_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--mempool.max-pending-chunk-items",
            "1000",
            "--mempool.max-chunks-per-item",
            "200",
            "--mempool.max-preheader-chunks-per-item",
            "100",
            "--mempool.max-preheader-data-path-bytes",
            "8192",
            "--mempool.max-valid-chunks",
            "10000",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.mempool.max_pending_chunk_items, 1000);
        assert_eq!(merged.mempool.max_chunks_per_item, 200);
        assert_eq!(merged.mempool.max_preheader_chunks_per_item, 100);
        assert_eq!(merged.mempool.max_preheader_data_path_bytes, 8192);
        assert_eq!(merged.mempool.max_valid_chunks, 10000);
    }

    #[test]
    fn test_mempool_commitment_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--mempool.max-valid-submit-txs",
            "6000",
            "--mempool.max-valid-commitment-addresses",
            "2000",
            "--mempool.max-commitments-per-address",
            "10",
            "--mempool.max-invalid-items",
            "10000",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.mempool.max_valid_submit_txs, 6000);
        assert_eq!(merged.mempool.max_valid_commitment_addresses, 2000);
        assert_eq!(merged.mempool.max_commitments_per_address, 10);
        assert_eq!(merged.mempool.max_invalid_items, 10000);
    }

    // -- P2P handshake tests --

    #[test]
    fn test_p2p_handshake_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--p2p.max-concurrent-handshakes",
            "64",
            "--p2p.max-peers-per-response",
            "50",
            "--p2p.max-retries",
            "16",
            "--p2p.backoff-base-secs",
            "2",
            "--p2p.backoff-cap-secs",
            "120",
            "--p2p.blocklist-ttl-secs",
            "1200",
            "--p2p.server-peer-list-cap",
            "50",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.p2p_handshake.max_concurrent_handshakes, 64);
        assert_eq!(merged.p2p_handshake.max_peers_per_response, 50);
        assert_eq!(merged.p2p_handshake.max_retries, 16);
        assert_eq!(merged.p2p_handshake.backoff_base_secs, 2);
        assert_eq!(merged.p2p_handshake.backoff_cap_secs, 120);
        assert_eq!(merged.p2p_handshake.blocklist_ttl_secs, 1200);
        assert_eq!(merged.p2p_handshake.server_peer_list_cap, 50);
    }

    // -- P2P gossip tests --

    #[test]
    fn test_p2p_gossip_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--p2p.broadcast-batch-size",
            "100",
            "--p2p.broadcast-throttle-interval",
            "200",
            "--p2p.enable-scoring",
            "false",
            "--p2p.max-concurrent-gossip-chunks",
            "100",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.p2p_gossip.broadcast_batch_size, 100);
        assert_eq!(merged.p2p_gossip.broadcast_batch_throttle_interval, 200);
        assert!(!merged.p2p_gossip.enable_scoring);
        assert_eq!(merged.p2p_gossip.max_concurrent_gossip_chunks, 100);
    }

    // -- P2P pull tests --

    #[test]
    fn test_p2p_pull_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--p2p.top-active-window",
            "20",
            "--p2p.sample-size",
            "10",
            "--p2p.max-attempts",
            "10",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.p2p_pull.top_active_window, 20);
        assert_eq!(merged.p2p_pull.sample_size, 10);
        assert_eq!(merged.p2p_pull.max_attempts, 10);
    }

    // -- Sync tests --

    #[test]
    fn test_sync_overrides() {
        let config = test_config();
        let cmd = parse_node_cmd(&[
            "--sync.block-batch-size",
            "100",
            "--sync.check-interval",
            "60",
            "--sync.retry-timeout",
            "60",
            "--sync.enable-periodic-check",
            "false",
            "--sync.queue-slot-timeout",
            "60",
            "--sync.queue-slot-max-attempts",
            "5",
        ]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.sync.block_batch_size, 100);
        assert_eq!(merged.sync.periodic_sync_check_interval_secs, 60);
        assert_eq!(merged.sync.retry_block_request_timeout_secs, 60);
        assert!(!merged.sync.enable_periodic_sync_check);
        assert_eq!(merged.sync.wait_queue_slot_timeout_secs, 60);
        assert_eq!(merged.sync.wait_queue_slot_max_attempts, 5);
    }

    // -- Top-level node arg tests --

    #[test]
    fn test_stake_pledge_drives_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--stake-pledge-drives", "true"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert!(merged.stake_pledge_drives);
    }

    #[test]
    fn test_genesis_peer_discovery_timeout_override() {
        let config = test_config();
        let cmd = parse_node_cmd(&["--genesis-peer-discovery-timeout", "5000"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.genesis_peer_discovery_timeout_millis, 5000);
    }
}
