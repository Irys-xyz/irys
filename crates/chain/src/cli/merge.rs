use std::path::PathBuf;

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
        // --genesis should win even if --node-mode peer is also given
        let config = test_config();
        let cmd = parse_node_cmd(&["--node-mode", "peer", "--genesis"]);
        let merged = apply_cli_overrides(config, &cmd).unwrap();
        assert_eq!(merged.node_mode, NodeMode::Genesis);
    }
}
