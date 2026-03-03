use std::path::PathBuf;
use std::time::Duration;

use clap::parser::ValueSource;
use eyre::{bail, WrapErr as _};
use irys_types::{ConsensusOptions, NodeConfig, NodeMode, PeerFilterMode, SyncMode};

use super::NodeCommand;

/// Merge CLI-provided values into config for a unified struct.
/// Only overrides fields explicitly set on the command line (not defaults).
macro_rules! merge_unified_struct {
    ($config:expr, $cli:expr, $matches:expr, [$($field:ident),* $(,)?]) => {
        $(
            if $matches.value_source(stringify!($field)) == Some(ValueSource::CommandLine) {
                $config.$field = $cli.$field.clone();
            }
        )*
    };
}

/// Apply an `Option`-based CLI override to a config field.
macro_rules! merge_option {
    // Value copy/clone: config.field = v
    ($config_field:expr, $cli_opt:expr) => {
        if let Some(v) = $cli_opt.clone() {
            $config_field = v;
        }
    };
    // Wrap in Some: config.field = Some(v)
    (some $config_field:expr, $cli_opt:expr) => {
        if let Some(v) = $cli_opt.clone() {
            $config_field = Some(v);
        }
    };
}

/// Apply CLI argument overrides to a loaded NodeConfig.
///
/// For unified config structs (CacheConfig, VdfNodeConfig, SyncConfig, etc.)
/// we use `ArgMatches::value_source()` to detect explicitly-set flags.
/// For adapter structs (NodeArgs, HttpArgs, etc.) we keep the `Option`-based pattern.
pub fn apply_cli_overrides(
    mut config: NodeConfig,
    cmd: &NodeCommand,
    matches: &clap::ArgMatches,
) -> eyre::Result<NodeConfig> {
    // -- Node args --
    merge_option!(config.base_directory, cmd.node.base_directory);
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
    merge_option!(config.stake_pledge_drives, cmd.node.stake_pledge_drives);
    merge_option!(
        config.genesis_peer_discovery_timeout_millis,
        cmd.node.genesis_peer_discovery_timeout_millis
    );

    if cmd.genesis {
        config.node_mode = NodeMode::Genesis;
    }

    // -- MINING_KEY env var - we explicitly do not allow mining_key to be passed in as a CLI arg --
    if let Ok(key_hex) = std::env::var("MINING_KEY") {
        let key_bytes =
            hex::decode(key_hex.trim()).wrap_err("MINING_KEY env var is not valid hex")?;
        let signing_key = k256::ecdsa::SigningKey::from_slice(&key_bytes)
            .wrap_err("MINING_KEY env var is not a valid secp256k1 private key")?;
        config.mining_key = signing_key;
    }

    // -- Network --
    merge_option!(config.network_defaults.public_ip, cmd.network.public_ip);
    merge_option!(config.network_defaults.bind_ip, cmd.network.bind_ip);

    // -- HTTP --
    merge_option!(config.http.bind_port, cmd.http.port);
    merge_option!(some config.http.bind_ip, cmd.http.bind_ip);
    merge_option!(config.http.public_port, cmd.http.public_port);

    // -- Gossip --
    merge_option!(config.gossip.bind_port, cmd.gossip.port);
    merge_option!(some config.gossip.bind_ip, cmd.gossip.bind_ip);
    merge_option!(config.gossip.public_port, cmd.gossip.public_port);

    // -- Reth network --
    merge_option!(config.reth.network.bind_port, cmd.reth.port);
    merge_option!(some config.reth.network.bind_ip, cmd.reth.bind_ip);
    merge_option!(config.reth.network.public_port, cmd.reth.public_port);

    // -- Reth RPC --
    merge_option!(config.reth.rpc.http, cmd.reth.rpc_http);
    merge_option!(config.reth.rpc.http_port, cmd.reth.rpc_http_port);
    merge_option!(config.reth.rpc.http_api, cmd.reth.rpc_http_api);
    merge_option!(config.reth.rpc.http_corsdomain, cmd.reth.rpc_corsdomain);
    merge_option!(config.reth.rpc.ws, cmd.reth.rpc_ws);
    merge_option!(config.reth.rpc.ws_port, cmd.reth.rpc_ws_port);
    merge_option!(config.reth.rpc.ws_api, cmd.reth.rpc_ws_api);
    merge_option!(
        config.reth.rpc.max_request_size_mb,
        cmd.reth.rpc_max_request_size_mb
    );
    merge_option!(
        config.reth.rpc.max_response_size_mb,
        cmd.reth.rpc_max_response_size_mb
    );
    merge_option!(
        config.reth.rpc.max_connections,
        cmd.reth.rpc_max_connections
    );
    merge_option!(config.reth.rpc.gas_cap, cmd.reth.rpc_gas_cap);
    merge_option!(config.reth.rpc.tx_fee_cap, cmd.reth.rpc_tx_fee_cap);

    // -- Reth txpool --
    merge_option!(
        config.reth.txpool.pending_max_count,
        cmd.reth.txpool_pending_max_count
    );
    merge_option!(
        config.reth.txpool.pending_max_size_mb,
        cmd.reth.txpool_pending_max_size_mb
    );
    merge_option!(
        config.reth.txpool.basefee_max_count,
        cmd.reth.txpool_basefee_max_count
    );
    merge_option!(
        config.reth.txpool.basefee_max_size_mb,
        cmd.reth.txpool_basefee_max_size_mb
    );
    merge_option!(
        config.reth.txpool.queued_max_count,
        cmd.reth.txpool_queued_max_count
    );
    merge_option!(
        config.reth.txpool.queued_max_size_mb,
        cmd.reth.txpool_queued_max_size_mb
    );
    merge_option!(
        config.reth.txpool.additional_validation_tasks,
        cmd.reth.txpool_additional_validation_tasks
    );
    merge_option!(
        config.reth.txpool.max_account_slots,
        cmd.reth.txpool_max_account_slots
    );
    merge_option!(config.reth.txpool.price_bump, cmd.reth.txpool_price_bump);

    // -- Reth engine --
    merge_option!(
        config.reth.engine.persistence_threshold,
        cmd.reth.engine_persistence_threshold
    );
    merge_option!(
        config.reth.engine.memory_block_buffer_target,
        cmd.reth.engine_memory_block_buffer_target
    );

    // -- Reth metrics --
    merge_option!(config.reth.metrics.port, cmd.reth.metrics_port);

    // -- Peer filter (enum) --
    if let Some(ref mode) = cmd.peer.peer_filter_mode {
        config.peer_filter_mode = parse_peer_filter_mode(mode)?;
    }

    // -- Storage --
    merge_option!(
        config.storage.num_writes_before_sync,
        cmd.storage.num_writes_before_sync
    );
    merge_option!(
        config.data_sync.max_pending_chunk_requests,
        cmd.storage.max_pending_chunk_requests
    );
    merge_option!(
        config.data_sync.max_storage_throughput_bps,
        cmd.storage.max_storage_throughput_bps
    );
    if let Some(secs) = cmd.storage.bandwidth_adjustment_interval_secs {
        config.data_sync.bandwidth_adjustment_interval = Duration::from_secs(secs);
    }
    if let Some(secs) = cmd.storage.chunk_request_timeout_secs {
        config.data_sync.chunk_request_timeout = Duration::from_secs(secs);
    }

    // ====================================================================
    // Unified config structs — use value_source() to detect CLI-provided values
    // ====================================================================

    merge_unified_struct!(
        config.packing.local,
        cmd.packing,
        matches,
        [cpu_packing_concurrency, gpu_packing_batch_size,]
    );

    merge_unified_struct!(
        config.cache,
        cmd.cache,
        matches,
        [
            cache_clean_lag,
            max_cache_size_bytes,
            prune_at_capacity_percent,
        ]
    );

    merge_unified_struct!(
        config.vdf,
        cmd.vdf,
        matches,
        [parallel_verification_thread_limit,]
    );

    merge_unified_struct!(
        config.mempool,
        cmd.mempool,
        matches,
        [
            max_pending_pledge_items,
            max_pledges_per_item,
            max_pending_chunk_items,
            max_chunks_per_item,
            max_preheader_chunks_per_item,
            max_preheader_data_path_bytes,
            max_valid_items,
            max_invalid_items,
            max_valid_chunks,
            max_valid_submit_txs,
            max_valid_commitment_addresses,
            max_commitments_per_address,
            max_concurrent_mempool_tasks,
            chunk_writer_buffer_size,
        ]
    );

    merge_unified_struct!(
        config.p2p_handshake,
        cmd.p2p_handshake,
        matches,
        [
            max_concurrent_handshakes,
            max_peers_per_response,
            max_retries,
            backoff_base_secs,
            backoff_cap_secs,
            blocklist_ttl_secs,
            server_peer_list_cap,
        ]
    );

    merge_unified_struct!(
        config.p2p_gossip,
        cmd.p2p_gossip,
        matches,
        [
            broadcast_batch_size,
            broadcast_batch_throttle_interval,
            enable_scoring,
            max_concurrent_gossip_chunks,
        ]
    );

    merge_unified_struct!(
        config.p2p_pull,
        cmd.p2p_pull,
        matches,
        [top_active_window, sample_size, max_attempts,]
    );

    merge_unified_struct!(
        config.sync,
        cmd.sync,
        matches,
        [
            block_batch_size,
            periodic_sync_check_interval_secs,
            retry_block_request_timeout_secs,
            enable_periodic_sync_check,
            wait_queue_slot_timeout_secs,
            wait_queue_slot_max_attempts,
        ]
    );

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
    use clap::{CommandFactory as _, FromArgMatches as _};

    fn test_config() -> NodeConfig {
        NodeConfig::testing()
    }

    /// Parse CLI args and return both the NodeCommand and the node-level ArgMatches.
    fn parse_node_cmd(args: &[&str]) -> (NodeCommand, clap::ArgMatches) {
        let mut full_args = vec!["irys", "node"];
        full_args.extend_from_slice(args);
        let matches = IrysCli::command().try_get_matches_from(full_args).unwrap();
        let cli = IrysCli::from_arg_matches(&matches).unwrap();
        let node_matches = matches
            .subcommand_matches("node")
            .expect("Node subcommand must have matches")
            .clone();
        match cli.command {
            Some(super::super::Commands::Node(cmd)) => (*cmd, node_matches),
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_no_overrides_preserves_config() {
        let config = test_config();
        let original_port = config.http.bind_port;
        let (cmd, matches) = parse_node_cmd(&[]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.http.bind_port, original_port);
    }

    #[test]
    fn test_http_port_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--http.port", "9080"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.http.bind_port, 9080);
    }

    #[test]
    fn test_gossip_port_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--gossip.port", "4444"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.gossip.bind_port, 4444);
    }

    #[test]
    fn test_reth_port_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--reth.port", "30304"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.network.bind_port, 30304);
    }

    #[test]
    fn test_genesis_flag() {
        let mut config = test_config();
        config.node_mode = NodeMode::Peer;
        let (cmd, matches) = parse_node_cmd(&["--genesis"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Genesis);
    }

    #[test]
    fn test_node_mode_override() {
        let mut config = test_config();
        config.node_mode = NodeMode::Genesis;
        let (cmd, matches) = parse_node_cmd(&["--node-mode", "peer"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Peer);
    }

    #[test]
    fn test_sync_mode_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--sync-mode", "full"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.sync_mode, SyncMode::Full);
    }

    #[test]
    fn test_consensus_override_testnet() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--consensus", "testnet"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert!(matches!(merged.consensus, ConsensusOptions::Testnet));
    }

    #[test]
    fn test_consensus_override_path() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--consensus", "/custom/path.toml"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert!(
            matches!(merged.consensus, ConsensusOptions::Path(p) if p == std::path::Path::new("/custom/path.toml"))
        );
    }

    #[test]
    fn test_peer_filter_mode_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--peer-filter-mode", "trusted-only"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.peer_filter_mode, PeerFilterMode::TrustedOnly);
    }

    #[test]
    fn test_peer_filter_mode_accepts_underscores() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--peer-filter-mode", "trusted_and_handshake"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.peer_filter_mode, PeerFilterMode::TrustedAndHandshake);
    }

    #[test]
    fn test_network_defaults_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--network.public-ip",
            "1.2.3.4",
            "--network.bind-ip",
            "0.0.0.0",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.network_defaults.public_ip, "1.2.3.4");
        assert_eq!(merged.network_defaults.bind_ip, "0.0.0.0");
    }

    #[test]
    fn test_bind_ip_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--http.bind-ip",
            "10.0.0.1",
            "--gossip.bind-ip",
            "10.0.0.2",
            "--reth.bind-ip",
            "10.0.0.3",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.http.bind_ip.as_deref(), Some("10.0.0.1"));
        assert_eq!(merged.gossip.bind_ip.as_deref(), Some("10.0.0.2"));
        assert_eq!(merged.reth.network.bind_ip.as_deref(), Some("10.0.0.3"));
    }

    #[test]
    fn test_public_port_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--http.public-port",
            "8080",
            "--gossip.public-port",
            "4000",
            "--reth.public-port",
            "30000",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.http.public_port, 8080);
        assert_eq!(merged.gossip.public_port, 4000);
        assert_eq!(merged.reth.network.public_port, 30000);
    }

    #[test]
    fn test_datadir_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--datadir", "/tmp/irys-data"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.base_directory, PathBuf::from("/tmp/irys-data"));
    }

    #[test]
    fn test_invalid_node_mode() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--node-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd, &matches).is_err());
    }

    #[test]
    fn test_invalid_sync_mode() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--sync-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd, &matches).is_err());
    }

    #[test]
    fn test_invalid_peer_filter_mode() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--peer-filter-mode", "invalid"]);
        assert!(apply_cli_overrides(config, &cmd, &matches).is_err());
    }

    #[test]
    fn test_multiple_overrides_combined() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--http.port",
            "9090",
            "--gossip.port",
            "5555",
            "--node-mode",
            "genesis",
            "--sync-mode",
            "full",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.http.bind_port, 9090);
        assert_eq!(merged.gossip.bind_port, 5555);
        assert_eq!(merged.node_mode, NodeMode::Genesis);
        assert_eq!(merged.sync_mode, SyncMode::Full);
    }

    #[test]
    fn test_genesis_flag_overrides_node_mode_arg() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--node-mode", "peer", "--genesis"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Genesis);
    }

    // -- Storage & data sync tests --

    #[test]
    fn test_storage_sync_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--storage.num-writes-before-sync", "500"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.storage.num_writes_before_sync, 500);
    }

    #[test]
    fn test_data_sync_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--storage.max-pending-chunk-requests",
            "2000",
            "--storage.max-throughput-bps",
            "104857600",
            "--storage.bandwidth-adjust-interval",
            "10",
            "--storage.chunk-request-timeout",
            "30",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
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
        let (cmd, matches) = parse_node_cmd(&[
            "--packing.cpu-concurrency",
            "8",
            "--packing.gpu-batch-size",
            "256",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.packing.local.cpu_packing_concurrency, 8);
        assert_eq!(merged.packing.local.gpu_packing_batch_size, 256);
    }

    #[test]
    fn test_packing_defaults_not_overridden() {
        let mut config = test_config();
        config.packing.local.cpu_packing_concurrency = 99;
        config.packing.local.gpu_packing_batch_size = 999;
        let (cmd, matches) = parse_node_cmd(&[]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        // value_source is DefaultValue, so config values should be preserved
        assert_eq!(merged.packing.local.cpu_packing_concurrency, 99);
        assert_eq!(merged.packing.local.gpu_packing_batch_size, 999);
    }

    // -- Cache tests --

    #[test]
    fn test_cache_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--cache.clean-lag",
            "5",
            "--cache.max-size-bytes",
            "5368709120",
            "--cache.prune-at-percent",
            "90.5",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.cache.cache_clean_lag, 5);
        assert_eq!(merged.cache.max_cache_size_bytes, 5368709120);
        assert!((merged.cache.prune_at_capacity_percent - 90.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cache_defaults_not_overridden() {
        let mut config = test_config();
        config.cache.cache_clean_lag = 99;
        let (cmd, matches) = parse_node_cmd(&[]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.cache.cache_clean_lag, 99);
    }

    // -- VDF tests --

    #[test]
    fn test_vdf_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--vdf.parallel-threads", "8"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.vdf.parallel_verification_thread_limit, 8);
    }

    // -- Mempool tests --

    #[test]
    fn test_mempool_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
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
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.mempool.max_pending_pledge_items, 2000);
        assert_eq!(merged.mempool.max_pledges_per_item, 20);
        assert_eq!(merged.mempool.max_valid_items, 50000);
        assert_eq!(merged.mempool.max_concurrent_mempool_tasks, 60);
        assert_eq!(merged.mempool.chunk_writer_buffer_size, 8192);
    }

    #[test]
    fn test_mempool_chunk_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
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
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.mempool.max_pending_chunk_items, 1000);
        assert_eq!(merged.mempool.max_chunks_per_item, 200);
        assert_eq!(merged.mempool.max_preheader_chunks_per_item, 100);
        assert_eq!(merged.mempool.max_preheader_data_path_bytes, 8192);
        assert_eq!(merged.mempool.max_valid_chunks, 10000);
    }

    #[test]
    fn test_mempool_commitment_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--mempool.max-valid-submit-txs",
            "6000",
            "--mempool.max-valid-commitment-addresses",
            "2000",
            "--mempool.max-commitments-per-address",
            "10",
            "--mempool.max-invalid-items",
            "10000",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.mempool.max_valid_submit_txs, 6000);
        assert_eq!(merged.mempool.max_valid_commitment_addresses, 2000);
        assert_eq!(merged.mempool.max_commitments_per_address, 10);
        assert_eq!(merged.mempool.max_invalid_items, 10000);
    }

    // -- P2P handshake tests --

    #[test]
    fn test_p2p_handshake_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
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
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
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
        let (cmd, matches) = parse_node_cmd(&[
            "--p2p.broadcast-batch-size",
            "100",
            "--p2p.broadcast-throttle-interval",
            "200",
            "--p2p.enable-scoring",
            "false",
            "--p2p.max-concurrent-gossip-chunks",
            "100",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.p2p_gossip.broadcast_batch_size, 100);
        assert_eq!(merged.p2p_gossip.broadcast_batch_throttle_interval, 200);
        assert!(!merged.p2p_gossip.enable_scoring);
        assert_eq!(merged.p2p_gossip.max_concurrent_gossip_chunks, 100);
    }

    // -- P2P pull tests --

    #[test]
    fn test_p2p_pull_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--p2p.top-active-window",
            "20",
            "--p2p.sample-size",
            "10",
            "--p2p.max-attempts",
            "10",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.p2p_pull.top_active_window, 20);
        assert_eq!(merged.p2p_pull.sample_size, 10);
        assert_eq!(merged.p2p_pull.max_attempts, 10);
    }

    // -- Sync tests --

    #[test]
    fn test_sync_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
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
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
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
        let (cmd, matches) = parse_node_cmd(&["--stake-pledge-drives", "true"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert!(merged.stake_pledge_drives);
    }

    #[test]
    fn test_genesis_peer_discovery_timeout_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--genesis-peer-discovery-timeout", "5000"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.genesis_peer_discovery_timeout_millis, 5000);
    }

    // -- Reth RPC tests --

    #[test]
    fn test_reth_rpc_http_port_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--reth.rpc-http-port", "9545"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.rpc.http_port, 9545);
    }

    #[test]
    fn test_reth_rpc_modules_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--reth.rpc-http-api", "eth,debug,net,trace"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.rpc.http_api, "eth,debug,net,trace");
    }

    #[test]
    fn test_reth_rpc_cors_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--reth.rpc-cors", "http://localhost:3000"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.rpc.http_corsdomain, "http://localhost:3000");
    }

    #[test]
    fn test_reth_rpc_ws_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--reth.rpc-ws",
            "true",
            "--reth.rpc-ws-port",
            "9546",
            "--reth.rpc-ws-api",
            "eth,net",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert!(merged.reth.rpc.ws);
        assert_eq!(merged.reth.rpc.ws_port, 9546);
        assert_eq!(merged.reth.rpc.ws_api, "eth,net");
    }

    #[test]
    fn test_reth_rpc_limits_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--reth.rpc-max-request-size",
            "30",
            "--reth.rpc-max-response-size",
            "320",
            "--reth.rpc-max-connections",
            "1000",
            "--reth.rpc-gas-cap",
            "100000000",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.rpc.max_request_size_mb, 30);
        assert_eq!(merged.reth.rpc.max_response_size_mb, 320);
        assert_eq!(merged.reth.rpc.max_connections, 1000);
        assert_eq!(merged.reth.rpc.gas_cap, 100_000_000);
    }

    // -- Reth txpool tests --

    #[test]
    fn test_reth_txpool_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--reth.txpool-pending-max-count",
            "500000",
            "--reth.txpool-pending-max-size",
            "500",
            "--reth.txpool-basefee-max-count",
            "500000",
            "--reth.txpool-basefee-max-size",
            "500",
            "--reth.txpool-queued-max-count",
            "500000",
            "--reth.txpool-queued-max-size",
            "500",
            "--reth.txpool-validation-tasks",
            "4",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.txpool.pending_max_count, 500_000);
        assert_eq!(merged.reth.txpool.pending_max_size_mb, 500);
        assert_eq!(merged.reth.txpool.basefee_max_count, 500_000);
        assert_eq!(merged.reth.txpool.basefee_max_size_mb, 500);
        assert_eq!(merged.reth.txpool.queued_max_count, 500_000);
        assert_eq!(merged.reth.txpool.queued_max_size_mb, 500);
        assert_eq!(merged.reth.txpool.additional_validation_tasks, 4);
    }

    #[test]
    fn test_reth_txpool_account_slots_and_price_bump() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--reth.txpool-max-account-slots",
            "32",
            "--reth.txpool-price-bump",
            "20",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.txpool.max_account_slots, 32);
        assert_eq!(merged.reth.txpool.price_bump, 20);
    }

    // -- Reth engine tests --

    #[test]
    fn test_reth_engine_overrides() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&[
            "--reth.engine-persistence-threshold",
            "10",
            "--reth.engine-memory-block-buffer",
            "5",
        ]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.engine.persistence_threshold, 10);
        assert_eq!(merged.reth.engine.memory_block_buffer_target, 5);
    }

    // -- Reth metrics tests --

    #[test]
    fn test_reth_metrics_port_override() {
        let config = test_config();
        let (cmd, matches) = parse_node_cmd(&["--reth.metrics-port", "9090"]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();
        assert_eq!(merged.reth.metrics.port, 9090);
    }

    // -- value_source tests: verify config not overridden when only defaults are used --

    #[test]
    fn test_unified_structs_preserve_config_when_no_cli_flags() {
        let mut config = test_config();
        // Set config values different from CLI defaults
        config.cache.cache_clean_lag = 99;
        config.vdf.parallel_verification_thread_limit = 99;
        config.sync.block_batch_size = 99;
        config.mempool.max_pending_pledge_items = 99;
        config.packing.local.cpu_packing_concurrency = 99;
        config.p2p_handshake.max_concurrent_handshakes = 99;
        config.p2p_gossip.broadcast_batch_size = 99;
        config.p2p_pull.top_active_window = 99;

        let (cmd, matches) = parse_node_cmd(&[]);
        let merged = apply_cli_overrides(config, &cmd, &matches).unwrap();

        // All config values should be preserved (not overwritten by CLI defaults)
        assert_eq!(merged.cache.cache_clean_lag, 99);
        assert_eq!(merged.vdf.parallel_verification_thread_limit, 99);
        assert_eq!(merged.sync.block_batch_size, 99);
        assert_eq!(merged.mempool.max_pending_pledge_items, 99);
        assert_eq!(merged.packing.local.cpu_packing_concurrency, 99);
        assert_eq!(merged.p2p_handshake.max_concurrent_handshakes, 99);
        assert_eq!(merged.p2p_gossip.broadcast_batch_size, 99);
        assert_eq!(merged.p2p_pull.top_active_window, 99);
    }
}
