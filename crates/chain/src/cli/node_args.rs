use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
pub struct NodeArgs {
    /// Base data directory for the node
    #[arg(long = "datadir", value_name = "DIR")]
    pub base_directory: Option<PathBuf>,

    /// Node mode: "genesis" or "peer"
    #[arg(long = "node-mode", value_name = "MODE")]
    pub node_mode: Option<String>,

    /// Sync mode: "trusted" or "full"
    #[arg(long = "sync-mode", value_name = "MODE")]
    pub sync_mode: Option<String>,

    /// Consensus config: "testing", "testnet", "mainnet", or a file path
    #[arg(long = "consensus", value_name = "CONSENSUS")]
    pub consensus: Option<String>,

    /// Reward address (also reads from REWARD_ADDRESS env var)
    #[arg(
        long = "reward-address",
        value_name = "ADDRESS",
        env = "REWARD_ADDRESS"
    )]
    pub reward_address: Option<String>,

    /// Enable stake-pledge-drives mode
    #[arg(long = "stake-pledge-drives")]
    pub stake_pledge_drives: Option<bool>,

    /// Genesis peer discovery timeout in milliseconds
    #[arg(
        id = "node.genesis_peer_discovery_timeout_millis",
        long = "genesis-peer-discovery-timeout",
        value_name = "MS"
    )]
    pub genesis_peer_discovery_timeout_millis: Option<u64>,
}
