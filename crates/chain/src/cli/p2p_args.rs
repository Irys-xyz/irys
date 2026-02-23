use clap::Parser;

#[derive(Debug, Parser)]
pub struct P2pArgs {
    // -- Handshake --
    /// Max concurrent P2P handshakes
    #[arg(
        id = "p2p.max-concurrent-handshakes",
        long = "p2p.max-concurrent-handshakes",
        value_name = "N"
    )]
    pub max_concurrent_handshakes: Option<usize>,

    /// Max peers returned per handshake response
    #[arg(
        id = "p2p.max-peers-per-response",
        long = "p2p.max-peers-per-response",
        value_name = "N"
    )]
    pub max_peers_per_response: Option<usize>,

    /// Max handshake retries
    #[arg(id = "p2p.max-retries", long = "p2p.max-retries", value_name = "N")]
    pub max_retries: Option<u32>,

    /// Handshake backoff base in seconds
    #[arg(
        id = "p2p.backoff-base-secs",
        long = "p2p.backoff-base-secs",
        value_name = "SECS"
    )]
    pub backoff_base_secs: Option<u64>,

    /// Handshake backoff cap in seconds
    #[arg(
        id = "p2p.backoff-cap-secs",
        long = "p2p.backoff-cap-secs",
        value_name = "SECS"
    )]
    pub backoff_cap_secs: Option<u64>,

    /// Peer blocklist TTL in seconds
    #[arg(
        id = "p2p.blocklist-ttl-secs",
        long = "p2p.blocklist-ttl-secs",
        value_name = "SECS"
    )]
    pub blocklist_ttl_secs: Option<u64>,

    /// Server peer list capacity
    #[arg(
        id = "p2p.server-peer-list-cap",
        long = "p2p.server-peer-list-cap",
        value_name = "N"
    )]
    pub server_peer_list_cap: Option<usize>,

    // -- Gossip --
    /// Max peers per gossip broadcast batch
    #[arg(
        id = "p2p.broadcast-batch-size",
        long = "p2p.broadcast-batch-size",
        value_name = "N"
    )]
    pub broadcast_batch_size: Option<usize>,

    /// Gossip broadcast throttle interval in milliseconds
    #[arg(
        id = "p2p.broadcast-throttle-interval",
        long = "p2p.broadcast-throttle-interval",
        value_name = "MS"
    )]
    pub broadcast_batch_throttle_interval: Option<u64>,

    /// Enable peer scoring based on behavior
    #[arg(
        id = "p2p.enable-scoring",
        long = "p2p.enable-scoring",
        value_name = "BOOL"
    )]
    pub enable_scoring: Option<bool>,

    /// Max concurrent inbound gossip chunk handler tasks
    #[arg(
        id = "p2p.max-concurrent-gossip-chunks",
        long = "p2p.max-concurrent-gossip-chunks",
        value_name = "N"
    )]
    pub max_concurrent_gossip_chunks: Option<usize>,

    // -- Pull --
    /// Top active peers window for pull requests
    #[arg(
        id = "p2p.top-active-window",
        long = "p2p.top-active-window",
        value_name = "N"
    )]
    pub top_active_window: Option<usize>,

    /// Peers to sample per pull batch
    #[arg(id = "p2p.sample-size", long = "p2p.sample-size", value_name = "N")]
    pub sample_size: Option<usize>,

    /// Max attempts per pull iteration
    #[arg(id = "p2p.max-attempts", long = "p2p.max-attempts", value_name = "N")]
    pub max_attempts: Option<u32>,
}
