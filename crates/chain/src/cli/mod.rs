mod gossip_args;
mod http_args;
pub mod merge;
mod network_args;
mod node_args;
mod peer_args;
mod reth_args;
mod storage_args;

pub use gossip_args::GossipArgs;
pub use http_args::HttpArgs;
pub use network_args::NetworkArgs;
pub use node_args::NodeArgs;
pub use peer_args::PeerArgs;
pub use reth_args::RethArgs;
pub use storage_args::StorageArgs;

use clap::{Parser, Subcommand};
use irys_types::{
    CacheConfig, LocalPackingConfig, MempoolNodeConfig, P2PGossipConfig, P2PHandshakeConfig,
    P2PPullConfig, SyncConfig, VdfNodeConfig,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "irys", about = "Irys decentralized storage blockchain node")]
pub struct IrysCli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the Irys node
    Node(Box<NodeCommand>),
}

#[derive(Debug, Parser)]
pub struct NodeCommand {
    /// Path to the TOML config file
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Generate a default config file and exit
    #[arg(long)]
    pub generate_config: bool,

    /// Start in genesis mode (shorthand for --node-mode genesis)
    #[arg(long)]
    pub genesis: bool,

    #[command(flatten)]
    pub node: NodeArgs,

    #[command(flatten)]
    pub network: NetworkArgs,

    #[command(flatten)]
    pub http: HttpArgs,

    #[command(flatten)]
    pub gossip: GossipArgs,

    #[command(flatten)]
    pub reth: RethArgs,

    #[command(flatten)]
    pub peer: PeerArgs,

    #[command(flatten)]
    pub storage: StorageArgs,

    #[command(flatten)]
    pub packing: LocalPackingConfig,

    #[command(flatten)]
    pub cache: CacheConfig,

    #[command(flatten)]
    pub vdf: VdfNodeConfig,

    #[command(flatten)]
    pub mempool: MempoolNodeConfig,

    #[command(flatten)]
    pub p2p_handshake: P2PHandshakeConfig,

    #[command(flatten)]
    pub p2p_gossip: P2PGossipConfig,

    #[command(flatten)]
    pub p2p_pull: P2PPullConfig,

    #[command(flatten)]
    pub sync: SyncConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bare_irys_parses_to_no_command() {
        let cli = IrysCli::try_parse_from(["irys"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_node_subcommand_parses() {
        let cli = IrysCli::try_parse_from(["irys", "node"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Node(_))));
    }

    #[test]
    fn test_node_with_config_flag() {
        let cli = IrysCli::try_parse_from(["irys", "node", "--config", "my.toml"]).unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.config, Some(PathBuf::from("my.toml")));
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_genesis_flag() {
        let cli = IrysCli::try_parse_from(["irys", "node", "--genesis"]).unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert!(cmd.genesis);
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_all_port_flags() {
        let cli = IrysCli::try_parse_from([
            "irys",
            "node",
            "--http.port",
            "8080",
            "--gossip.port",
            "4000",
            "--reth.port",
            "30303",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.http.port, Some(8080));
                assert_eq!(cmd.gossip.port, Some(4000));
                assert_eq!(cmd.reth.port, Some(30303));
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_generate_config() {
        let cli = IrysCli::try_parse_from(["irys", "node", "--generate-config"]).unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert!(cmd.generate_config);
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_all_bind_ips() {
        let cli = IrysCli::try_parse_from([
            "irys",
            "node",
            "--network.public-ip",
            "1.2.3.4",
            "--network.bind-ip",
            "0.0.0.0",
            "--http.bind-ip",
            "127.0.0.1",
            "--gossip.bind-ip",
            "127.0.0.1",
            "--reth.bind-ip",
            "127.0.0.1",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.network.public_ip, Some("1.2.3.4".to_string()));
                assert_eq!(cmd.network.bind_ip, Some("0.0.0.0".to_string()));
                assert_eq!(cmd.http.bind_ip, Some("127.0.0.1".to_string()));
                assert_eq!(cmd.gossip.bind_ip, Some("127.0.0.1".to_string()));
                assert_eq!(cmd.reth.bind_ip, Some("127.0.0.1".to_string()));
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_storage_flags() {
        let cli = IrysCli::try_parse_from([
            "irys",
            "node",
            "--storage.num-writes-before-sync",
            "200",
            "--storage.max-pending-chunk-requests",
            "500",
            "--storage.max-throughput-bps",
            "104857600",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.storage.num_writes_before_sync, Some(200));
                assert_eq!(cmd.storage.max_pending_chunk_requests, Some(500));
                assert_eq!(cmd.storage.max_storage_throughput_bps, Some(104857600));
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_p2p_flags() {
        let cli = IrysCli::try_parse_from([
            "irys",
            "node",
            "--p2p.max-concurrent-handshakes",
            "64",
            "--p2p.broadcast-batch-size",
            "100",
            "--p2p.enable-scoring",
            "false",
            "--p2p.sample-size",
            "10",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.p2p_handshake.max_concurrent_handshakes, 64);
                assert_eq!(cmd.p2p_gossip.broadcast_batch_size, 100);
                assert!(!cmd.p2p_gossip.enable_scoring);
                assert_eq!(cmd.p2p_pull.sample_size, 10);
            }
            _ => panic!("expected Node command"),
        }
    }

    #[test]
    fn test_node_with_sync_flags() {
        let cli = IrysCli::try_parse_from([
            "irys",
            "node",
            "--sync.block-batch-size",
            "100",
            "--sync.enable-periodic-check",
            "false",
            "--sync.retry-timeout",
            "60",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Node(cmd)) => {
                assert_eq!(cmd.sync.block_batch_size, 100);
                assert!(!cmd.sync.enable_periodic_sync_check);
                assert_eq!(cmd.sync.retry_block_request_timeout_secs, 60);
            }
            _ => panic!("expected Node command"),
        }
    }
}
