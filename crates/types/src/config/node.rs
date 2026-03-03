use crate::{
    irys::IrysSigner,
    storage_pricing::{
        phantoms::{IrysPrice, Usd},
        Amount,
    },
    ConsensusConfig, IrysAddress, MempoolConfig, PeerAddress, RethPeerInfo, VdfConfig, H256,
};
use crate::{serde_utils, ConsensusOptions};
#[cfg(any(test, feature = "test-utils"))]
use alloy_genesis::GenesisAccount;

use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::{env, path::PathBuf, time::Duration};

/// # Node Configuration
///
/// The main configuration for an Irys node, containing all settings needed
/// to participate in the network. This includes network mode, consensus rules,
/// pricing parameters, and system resource allocations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Determines how the node joins and interacts with the network
    pub node_mode: NodeMode,

    /// The synchronization mode for the node
    pub sync_mode: SyncMode,

    /// The base directory where to look for artifact data
    #[serde(default = "default_irys_path")]
    pub base_directory: PathBuf,

    /// Private key used for mining operations
    /// This key identifies the node and receives mining rewards
    #[serde(
        deserialize_with = "serde_utils::signing_key_from_hex",
        serialize_with = "serde_utils::serializes_signing_key"
    )]
    pub mining_key: k256::ecdsa::SigningKey,

    /// The initial list of peers to contact for block sync
    pub trusted_peers: Vec<PeerAddress>,

    /// Initial whitelist of miner who can post stake and pledge transaction. To be removed on a
    /// later date. If this field is empty, all peers are allowed to stake and pledge.
    /// This has effect only on the genesis node, as all other nodes will get this parameter
    /// from their trusted peers.
    #[serde(default)]
    pub initial_stake_and_pledge_whitelist: Vec<IrysAddress>,

    /// Initial whitelist of peers to connect to. If you're joining the network as a peer in a
    /// trusted-only or trusted-and-handshake mode, you'll be supplied one during the handshake
    /// with the trusted peers. For the original trusted peer that has to be set.
    #[serde(default)]
    pub initial_whitelist: Vec<SocketAddr>,

    /// Controls how the node filters peer interactions
    #[serde(default = "default_peer_filter_mode")]
    pub peer_filter_mode: PeerFilterMode,

    pub reward_address: IrysAddress,

    // whether we should try to stake & pledge our local drives
    #[serde(default)]
    pub stake_pledge_drives: bool,

    #[serde(default = "default_genesis_peer_discovery_timeout_millis")]
    pub genesis_peer_discovery_timeout_millis: u64,

    /// Default network configuration used by all services unless overridden
    #[serde(default = "default_network_defaults")]
    pub network_defaults: NetworkDefaults,

    /// Peer-to-peer network communication settings
    pub gossip: GossipConfig,

    /// HTTP API server configuration
    pub http: HttpConfig,

    /// Reth node configuration
    pub reth: RethConfig,

    /// StorageModule configuration
    #[serde(default)]
    pub storage: StorageSyncConfig,

    /// DataSyncService configuration
    #[serde(default)]
    pub data_sync: DataSyncServiceConfig,

    /// Data packing and compression settings
    #[serde(default)]
    pub packing: PackingConfig,

    /// Cache management configuration
    #[serde(default)]
    pub cache: CacheConfig,

    /// Settings for the price oracle system (list).
    #[serde(default)]
    pub oracles: Vec<OracleConfig>,

    #[serde(default)]
    pub vdf: VdfNodeConfig,

    #[serde(default)]
    pub mempool: MempoolNodeConfig,

    /// Specifies which consensus rules the node follows
    pub consensus: ConsensusOptions,

    /// P2P handshake parameters
    #[serde(default)]
    pub p2p_handshake: P2PHandshakeConfig,

    /// Gossip/broadcast parameters
    #[serde(default)]
    pub p2p_gossip: P2PGossipConfig,

    /// P2P pull/request parameters
    #[serde(default)]
    pub p2p_pull: P2PPullConfig,

    /// Sync parameters - how many blocks to pull in parallel, timeouts, etc
    #[serde(default)]
    pub sync: SyncConfig,
}

/// # Node Operation Mode
///
/// Defines how the node participates in the network - either as a genesis node
/// that starts a new network or as a peer that syncs with existing nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum NodeMode {
    /// Start a new blockchain network as the first node
    Genesis,

    /// Join an existing network by connecting to trusted peers.
    /// Requires `consensus.expected_genesis_hash` to be set.
    Peer,
}

/// # Node Synchronization Mode
///
/// Defines the method the node uses to synchronize with the network.
/// Trusted mode allows for faster sync by relying on trusted peers,
/// while Full mode ensures complete validation of all blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SyncMode {
    /// Fast sync mode, downloads index from the trusted peers and skips
    /// heavy parts of the block validation
    Trusted,
    /// Full sync mode, fully validates all blocks
    Full,
}

/// # Peer Filter Mode
///
/// Defines how the node filters which peers it will interact with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerFilterMode {
    /// No restrictions - interact with any discovered peers (default behavior)
    Unrestricted,

    /// Only interact with peers specified in the `trusted_peers` list
    TrustedOnly,

    /// Interact with trusted peers and additional peers they return during handshake
    /// The combination of trusted peers + handshake peers forms the whitelist
    TrustedAndHandshake,
}

/// # Oracle Configuration
///
/// Defines how the node obtains and processes external price information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum OracleConfig {
    /// A simulated price oracle for testing and development
    #[serde(rename = "mock")]
    Mock {
        /// Starting price for the token in USD
        #[serde(
            deserialize_with = "serde_utils::token_amount",
            serialize_with = "serde_utils::serializes_token_amount"
        )]
        initial_price: Amount<(IrysPrice, Usd)>,

        /// How much the price can change between updates
        #[serde(
            deserialize_with = "serde_utils::token_amount",
            serialize_with = "serde_utils::serializes_token_amount"
        )]
        incremental_change: Amount<(IrysPrice, Usd)>,

        /// Number of blocks between price updates
        smoothing_interval: u64,
        /// Initial direction of price movement. When true the mock increases first;
        /// when false it decreases first.
        #[serde(default = "super::default_oracle_initial_direction_up")]
        initial_direction_up: bool,
        /// Poll interval in milliseconds for refreshing the mock oracle price snapshots.
        #[serde(default = "default_mock_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
    /// CoinMarketCap-backed price oracle
    #[serde(rename = "coinmarketcap", alias = "coin_market_cap")]
    CoinMarketCap {
        /// API key for the CoinMarketCap Pro API
        api_key: String,
        /// CoinMarketCap coin id (e.g., "1" for Bitcoin).
        /// Retrieve ids from https://api.coinmarketcap.com/data-api/v3/map/all?listing_status=active
        id: String,
        /// Poll interval in milliseconds.
        /// Free tier is limited to 10k requests/month, so a 5 minute (300_000 ms) interval is a safe default.
        #[serde(default = "default_price_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
    /// CoinGecko-backed price oracle
    #[serde(rename = "coingecko", alias = "coin_gecko")]
    CoinGecko {
        /// API key for the CoinGecko Pro/Demo API
        api_key: String,
        /// CoinGecko coin id (e.g., "bitcoin", "ethereum").
        /// Retrieve ids from https://docs.coingecko.com/reference/coins-list
        /// Or from the official spreadsheet: https://docs.google.com/spreadsheets/d/1wTTuxXt8n9q7C4NDXqQpI3wpKu1_5bGVmP9Xz0XGSyU/edit?gid=0#gid=0
        coin_id: String,
        /// Set to true when using a CoinGecko demo API key (free, not paid version)
        #[serde(default)]
        demo_api_key: bool,
        /// Poll interval in milliseconds.
        /// Free tier is limited to 10k requests/month, so a 5 minute (300_000 ms) interval is a safe default.
        #[serde(default = "default_price_oracle_poll_interval_ms")]
        poll_interval_ms: u64,
    },
}

const fn default_price_oracle_poll_interval_ms() -> u64 {
    300_000
}

const fn default_mock_oracle_poll_interval_ms() -> u64 {
    10_000
}

pub(crate) const fn default_oracle_initial_direction_up() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageSyncConfig {
    /// Number of write operations before forcing a sync to disk
    /// Higher values improve performance but increase data loss risk on crashes
    pub num_writes_before_sync: u64,
}

impl Default for StorageSyncConfig {
    fn default() -> Self {
        Self {
            num_writes_before_sync: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DataSyncServiceConfig {
    pub max_pending_chunk_requests: u64,
    pub max_storage_throughput_bps: u64,
    #[serde(
        deserialize_with = "serde_utils::duration_from_string",
        serialize_with = "serde_utils::serialize_duration_string"
    )]
    pub bandwidth_adjustment_interval: Duration,
    #[serde(
        deserialize_with = "serde_utils::duration_from_string",
        serialize_with = "serde_utils::serialize_duration_string"
    )]
    pub chunk_request_timeout: Duration,
}

impl Default for DataSyncServiceConfig {
    fn default() -> Self {
        Self {
            max_pending_chunk_requests: 1000,
            max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
            bandwidth_adjustment_interval: Duration::from_secs(5),
            chunk_request_timeout: Duration::from_secs(10),
        }
    }
}

/// # Network Defaults
///
/// Default IP addresses used across all services unless overridden.
/// This allows you to specify public_ip and bind_ip once instead of
/// repeating them for each service (http, gossip, reth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDefaults {
    /// Default public IP address advertised to other peers
    pub public_ip: String,
    /// Default bind IP address for all services
    pub bind_ip: String,
}

/// Network configuration with optional overrides
pub trait NetworkConfigWithDefaults {
    /// Get the optional public IP
    fn public_ip_option(&self) -> &Option<String>;

    /// Get the optional bind IP
    fn bind_ip_option(&self) -> &Option<String>;

    /// Get the public IP, falling back to network defaults if not set
    fn public_ip<'a>(&'a self, defaults: &'a NetworkDefaults) -> &'a str {
        self.public_ip_option()
            .as_deref()
            .unwrap_or(&defaults.public_ip)
    }

    /// Get the bind IP, falling back to network defaults if not set
    fn bind_ip<'a>(&'a self, defaults: &'a NetworkDefaults) -> &'a str {
        self.bind_ip_option()
            .as_deref()
            .unwrap_or(&defaults.bind_ip)
    }
}

/// Macro to implement NetworkConfigWithDefaults for types with public_ip and bind_ip fields
macro_rules! impl_network_config_with_defaults {
    ($type:ty) => {
        impl NetworkConfigWithDefaults for $type {
            fn public_ip_option(&self) -> &Option<String> {
                &self.public_ip
            }

            fn bind_ip_option(&self) -> &Option<String> {
                &self.bind_ip
            }
        }
    };
}

/// # Gossip Network Configuration
///
/// Settings for peer-to-peer communication between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GossipConfig {
    /// The IP address that's going to be announced to other peers
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address the gossip service binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port number the gossip service listens on
    pub bind_port: u16,
}

impl_network_config_with_defaults!(GossipConfig);

/// # Reth Node Configuration
///
/// Settings that are passed to the reth node
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethConfig {
    pub network: RethNetworkConfig,
    #[serde(default)]
    pub rpc: RethRpcConfig,
    #[serde(default)]
    pub txpool: RethTxPoolConfig,
    #[serde(default)]
    pub engine: RethEngineConfig,
    #[serde(default)]
    pub metrics: RethMetricsConfig,
}

/// # Reth RPC Configuration
///
/// Controls the JSON-RPC server exposed by the Reth EVM node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RethRpcConfig {
    /// Enable HTTP RPC server
    pub http: bool,
    /// HTTP RPC port (separate from the Irys HTTP API port)
    pub http_port: u16,
    /// Comma-separated list of RPC modules to enable (e.g. "eth,debug,net,trace")
    pub http_api: String,
    /// CORS domain for HTTP RPC ("*" for any)
    pub http_corsdomain: String,
    /// Enable WebSocket RPC server
    pub ws: bool,
    /// WebSocket RPC port
    pub ws_port: u16,
    /// Comma-separated list of RPC modules for WebSocket
    pub ws_api: String,
    /// Maximum request size in MB
    pub max_request_size_mb: u32,
    /// Maximum response size in MB
    pub max_response_size_mb: u32,
    /// Maximum concurrent RPC connections
    pub max_connections: u32,
    /// Gas cap for eth_call and related methods
    pub gas_cap: u64,
    /// Maximum transaction fee cap in wei (max ~18.4 ETH at u64 range)
    pub tx_fee_cap: u64,
}

impl Default for RethRpcConfig {
    fn default() -> Self {
        Self {
            http: true,
            http_port: 8545,
            http_api: "eth,debug".to_string(),
            http_corsdomain: "*".to_string(),
            ws: false,
            ws_port: 8546,
            ws_api: "eth".to_string(),
            max_request_size_mb: 15,
            max_response_size_mb: 160,
            max_connections: 500,
            gas_cap: 50_000_000,
            tx_fee_cap: 1_000_000_000_000_000_000, // 1 ETH
        }
    }
}

/// # Reth Transaction Pool Configuration
///
/// Controls sizing and behavior of the Reth EVM transaction pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RethTxPoolConfig {
    /// Maximum number of pending transactions
    pub pending_max_count: usize,
    /// Maximum size of pending pool in MB
    pub pending_max_size_mb: usize,
    /// Maximum number of basefee transactions
    pub basefee_max_count: usize,
    /// Maximum size of basefee pool in MB
    pub basefee_max_size_mb: usize,
    /// Maximum number of queued transactions
    pub queued_max_count: usize,
    /// Maximum size of queued pool in MB
    pub queued_max_size_mb: usize,
    /// Number of additional validation tasks
    pub additional_validation_tasks: usize,
    /// Maximum number of transaction slots per account
    pub max_account_slots: usize,
    /// Price bump percentage to replace existing transactions
    pub price_bump: u64,
}

impl Default for RethTxPoolConfig {
    fn default() -> Self {
        Self {
            pending_max_count: 1_000_000,
            pending_max_size_mb: 1000,
            basefee_max_count: 1_000_000,
            basefee_max_size_mb: 1000,
            queued_max_count: 1_000_000,
            queued_max_size_mb: 1000,
            additional_validation_tasks: 2,
            max_account_slots: 16,
            price_bump: 10,
        }
    }
}

/// # Reth Engine Configuration
///
/// Controls the Reth consensus engine behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RethEngineConfig {
    /// Persistence threshold — how many blocks to accumulate before flushing to disk
    pub persistence_threshold: u64,
    /// Target number of blocks to keep in memory
    pub memory_block_buffer_target: u64,
}

/// # Reth Metrics Configuration
///
/// Controls the Prometheus metrics endpoint for the Reth node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RethMetricsConfig {
    /// Prometheus metrics port (0 for random)
    pub port: u16,
}

impl Default for RethMetricsConfig {
    fn default() -> Self {
        Self { port: 9001 }
    }
}

/// # Reth network Configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethNetworkConfig {
    #[serde(default)]
    pub use_random_ports: bool,
    /// The IP address that's going to be announced to other peers
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address that Reth binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port number the Reth listens on
    pub bind_port: u16,
    // peer ID
    // WARNING: this gets overridden partway through the startup sequence with the correct value
    #[serde(default)]
    pub peer_id: reth_transaction_pool::PeerId,
}

impl_network_config_with_defaults!(RethNetworkConfig);

/// # Data Packing Configuration
///
/// Controls how data is compressed and packed for storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PackingConfig {
    #[serde(default)]
    pub local: LocalPackingConfig,
    #[serde(default)]
    pub remote: Vec<RemotePackingConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct LocalPackingConfig {
    /// Number of CPU threads to use for data packing operations
    #[arg(
        long = "packing.cpu-concurrency",
        default_value_t = Self::default().cpu_packing_concurrency
    )]
    pub cpu_packing_concurrency: u16,

    /// Batch size for GPU-accelerated packing operations
    #[arg(
        long = "packing.gpu-batch-size",
        default_value_t = Self::default().gpu_packing_batch_size
    )]
    pub gpu_packing_batch_size: u32,
}

impl Default for LocalPackingConfig {
    fn default() -> Self {
        Self {
            cpu_packing_concurrency: 2, // TODO: default to something like numcpus - 4
            gpu_packing_batch_size: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePackingConfig {
    pub url: String,

    // This is the read (max time between streamed chunks) and connection timeout
    pub timeout: Option<Duration>,
}

/// # Cache Configuration
///
/// Settings for in-memory caching to improve performance.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, clap::Args)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Number of blocks cache cleaning will lag behind block finalization
    /// Higher values keep more data in cache but use more memory
    #[arg(long = "cache.clean-lag", default_value_t = Self::default().cache_clean_lag)]
    pub cache_clean_lag: u8,

    #[arg(long = "cache.max-size-bytes", default_value_t = Self::default().max_cache_size_bytes)]
    pub max_cache_size_bytes: u64,

    /// Target capacity for chunk cache as a percentage of it's total capacity (0 -> 100%)
    /// Don't set this too low, or you won't be able to promote transactions
    #[arg(long = "cache.prune-at-percent", default_value_t = Self::default().prune_at_capacity_percent)]
    pub prune_at_capacity_percent: f64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            cache_clean_lag: 0,
            max_cache_size_bytes: 10_737_418_240,
            prune_at_capacity_percent: 80_f64,
        }
    }
}

/// # HTTP API Configuration
///
/// Settings for the node's HTTP server that provides API access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// The IP address visible to the outside world
    #[serde(default)]
    pub public_ip: Option<String>,
    /// The port that is visible to the outside world
    pub public_port: u16,
    /// The IP address the HTTP service binds to
    #[serde(default)]
    pub bind_ip: Option<String>,
    /// The port that the Node's HTTP server should listen on. Set to 0 for randomization.
    pub bind_port: u16,
}

impl_network_config_with_defaults!(HttpConfig);

/// P2P handshake configuration with sensible defaults
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct P2PHandshakeConfig {
    #[arg(
        long = "p2p.max-concurrent-handshakes",
        default_value_t = Self::default().max_concurrent_handshakes
    )]
    pub max_concurrent_handshakes: usize,
    #[arg(
        long = "p2p.max-peers-per-response",
        default_value_t = Self::default().max_peers_per_response
    )]
    pub max_peers_per_response: usize,
    #[arg(long = "p2p.max-retries", default_value_t = Self::default().max_retries)]
    pub max_retries: u32,
    #[arg(
        long = "p2p.backoff-base-secs",
        default_value_t = Self::default().backoff_base_secs
    )]
    pub backoff_base_secs: u64,
    #[arg(
        long = "p2p.backoff-cap-secs",
        default_value_t = Self::default().backoff_cap_secs
    )]
    pub backoff_cap_secs: u64,
    #[arg(
        long = "p2p.blocklist-ttl-secs",
        default_value_t = Self::default().blocklist_ttl_secs
    )]
    pub blocklist_ttl_secs: u64,
    #[arg(
        long = "p2p.server-peer-list-cap",
        default_value_t = Self::default().server_peer_list_cap
    )]
    pub server_peer_list_cap: usize,
}

impl Default for P2PHandshakeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_handshakes: 32,
            max_peers_per_response: 25,
            max_retries: 8,
            backoff_base_secs: 1,
            backoff_cap_secs: 60,
            blocklist_ttl_secs: 600,
            server_peer_list_cap: 25,
        }
    }
}

/// P2P gossip/broadcast configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct P2PGossipConfig {
    /// Maximum peers to target per broadcast step
    #[arg(
        long = "p2p.broadcast-batch-size",
        default_value_t = Self::default().broadcast_batch_size
    )]
    pub broadcast_batch_size: usize,
    /// Interval between broadcast steps in milliseconds
    #[arg(
        long = "p2p.broadcast-throttle-interval",
        default_value_t = Self::default().broadcast_batch_throttle_interval
    )]
    pub broadcast_batch_throttle_interval: u64,
    /// Enable scoring of peers based on their behavior. Disabling this might help with reducing
    /// noise during debug, otherwise it's recommended to keep it enabled.
    #[arg(long = "p2p.enable-scoring", default_value_t = Self::default().enable_scoring, action = clap::ArgAction::Set)]
    pub enable_scoring: bool,
    /// Maximum concurrent chunk handler tasks on the gossip receiver.
    /// Limits memory and CPU pressure from inbound chunk processing.
    #[arg(
        long = "p2p.max-concurrent-gossip-chunks",
        default_value_t = Self::default().max_concurrent_gossip_chunks
    )]
    pub max_concurrent_gossip_chunks: usize,
}

impl Default for P2PGossipConfig {
    fn default() -> Self {
        Self {
            broadcast_batch_size: 50,
            broadcast_batch_throttle_interval: 100,
            enable_scoring: true,
            max_concurrent_gossip_chunks: 50,
        }
    }
}

/// P2P pull/request configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct P2PPullConfig {
    /// How many top active peers to consider before random sampling
    #[arg(
        long = "p2p.top-active-window",
        default_value_t = Self::default().top_active_window
    )]
    pub top_active_window: usize,
    /// Number of peers to randomly sample (truncate) per pull attempt batch
    #[arg(long = "p2p.sample-size", default_value_t = Self::default().sample_size)]
    pub sample_size: usize,
    /// Maximum number of attempts to iterate over the sampled set
    #[arg(
        long = "p2p.max-attempts",
        default_value_t = Self::default().max_attempts
    )]
    pub max_attempts: u32,
}

impl Default for P2PPullConfig {
    fn default() -> Self {
        Self {
            top_active_window: 10,
            sample_size: 5,
            max_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    /// How many blocks to fetch in parallel per batch during the sync
    #[arg(
        long = "sync.block-batch-size",
        default_value_t = Self::default().block_batch_size
    )]
    pub block_batch_size: usize,
    /// How often to check if we're behind and need to sync
    #[arg(
        long = "sync.check-interval",
        default_value_t = Self::default().periodic_sync_check_interval_secs
    )]
    pub periodic_sync_check_interval_secs: u64,
    /// Timeout for retry block pull/process
    #[arg(
        long = "sync.retry-timeout",
        default_value_t = Self::default().retry_block_request_timeout_secs
    )]
    pub retry_block_request_timeout_secs: u64,
    /// Whether to enable periodic sync checks
    #[arg(long = "sync.enable-periodic-check", default_value_t = Self::default().enable_periodic_sync_check, action = clap::ArgAction::Set)]
    pub enable_periodic_sync_check: bool,
    /// Timeout per attempt when waiting for a queue slot
    #[arg(
        long = "sync.queue-slot-timeout",
        default_value_t = Self::default().wait_queue_slot_timeout_secs
    )]
    pub wait_queue_slot_timeout_secs: u64,
    /// Maximum consecutive timeout attempts when waiting for a queue slot with no active validations
    #[arg(
        long = "sync.queue-slot-max-attempts",
        default_value_t = Self::default().wait_queue_slot_max_attempts
    )]
    pub wait_queue_slot_max_attempts: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            block_batch_size: 50,
            periodic_sync_check_interval_secs: 30,
            retry_block_request_timeout_secs: 30,
            enable_periodic_sync_check: true,
            wait_queue_slot_timeout_secs: 30,
            wait_queue_slot_max_attempts: 3,
        }
    }
}

/// Default for `peer_filter_mode` when the field is not present in the provided TOML.
/// This keeps legacy configurations working by defaulting to unrestricted mode.
fn default_peer_filter_mode() -> PeerFilterMode {
    PeerFilterMode::Unrestricted
}

/// Default genesis peer discovery timeout (20s)
fn default_genesis_peer_discovery_timeout_millis() -> u64 {
    20_000
}

/// Default network configuration when not specified in the config file.
/// Uses localhost for public_ip and binds to all interfaces.
fn default_network_defaults() -> NetworkDefaults {
    NetworkDefaults {
        public_ip: "127.0.0.1".to_string(),
        bind_ip: "0.0.0.0".to_string(),
    }
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields)]
pub struct VdfNodeConfig {
    /// Maximum number of threads to use for parallel VDF verification
    #[arg(
        long = "vdf.parallel-threads",
        default_value_t = Self::default().parallel_verification_thread_limit
    )]
    pub parallel_verification_thread_limit: usize,
}

impl Default for VdfNodeConfig {
    fn default() -> Self {
        Self {
            // TODO: default to something like numcpus - 4
            parallel_verification_thread_limit: 4,
        }
    }
}

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
#[serde(deny_unknown_fields, default)]
pub struct MempoolNodeConfig {
    /// Maximum number of addresses in the LRU cache for out-of-order stakes and pledges
    /// Controls memory usage for tracking transactions that arrive before their dependencies
    #[arg(
        long = "mempool.max-pending-pledge-items",
        default_value_t = Self::default().max_pending_pledge_items
    )]
    pub max_pending_pledge_items: usize,

    /// Maximum number of pending pledge transactions allowed per address
    /// Limits the resources that can be consumed by a single address
    #[arg(
        long = "mempool.max-pledges-per-item",
        default_value_t = Self::default().max_pledges_per_item
    )]
    pub max_pledges_per_item: usize,

    /// Maximum number of transaction data roots to keep in the pending cache
    /// For transactions whose chunks arrive before the transaction header
    #[arg(
        long = "mempool.max-pending-chunk-items",
        default_value_t = Self::default().max_pending_chunk_items
    )]
    pub max_pending_chunk_items: usize,

    /// Maximum number of chunks that can be cached per data root
    /// Prevents memory exhaustion from excessive chunk storage for a single transaction
    #[arg(
        long = "mempool.max-chunks-per-item",
        default_value_t = Self::default().max_chunks_per_item
    )]
    pub max_chunks_per_item: usize,

    /// Maximum number of pre-header chunks to keep per data root before the header arrives
    /// Limits speculative storage window for out-of-order chunks
    #[arg(
        long = "mempool.max-preheader-chunks-per-item",
        default_value_t = Self::default().max_preheader_chunks_per_item
    )]
    pub max_preheader_chunks_per_item: usize,

    /// Maximum allowed pre-header data_path bytes for chunk proofs
    /// Mitigates DoS on speculative chunk storage before header arrival
    #[arg(
        long = "mempool.max-preheader-data-path-bytes",
        default_value_t = Self::default().max_preheader_data_path_bytes
    )]
    pub max_preheader_data_path_bytes: usize,

    /// Maximum number of valid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    #[arg(
        long = "mempool.max-valid-items",
        default_value_t = Self::default().max_valid_items
    )]
    pub max_valid_items: usize,

    /// Maximum number of invalid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    #[arg(
        long = "mempool.max-invalid-items",
        default_value_t = Self::default().max_invalid_items
    )]
    pub max_invalid_items: usize,

    /// Maximum number of valid chunk hashes to keep track of
    /// Prevents re-processing and re-gossipping of recently seen chunks
    #[arg(
        long = "mempool.max-valid-chunks",
        default_value_t = Self::default().max_valid_chunks
    )]
    pub max_valid_chunks: usize,

    /// Maximum number of data transactions to hold in mempool
    /// Prevents unbounded growth. Conservative: max_data_txs_per_block * block_migration_depth * 3
    #[arg(
        long = "mempool.max-valid-submit-txs",
        default_value_t = Self::default().max_valid_submit_txs
    )]
    pub max_valid_submit_txs: usize,

    /// Maximum number of addresses with pending commitment transactions
    /// Prevents unbounded growth. Conservative: num_staked_miners * 3
    #[arg(
        long = "mempool.max-valid-commitment-addresses",
        default_value_t = Self::default().max_valid_commitment_addresses
    )]
    pub max_valid_commitment_addresses: usize,

    /// Maximum commitment transactions per address
    /// Limits the resources that can be consumed by a single address
    #[arg(
        long = "mempool.max-commitments-per-address",
        default_value_t = Self::default().max_commitments_per_address
    )]
    pub max_commitments_per_address: usize,

    /// Maximum number of concurrent handlers for the mempool messages
    #[arg(
        long = "mempool.max-concurrent-tasks",
        default_value_t = Self::default().max_concurrent_mempool_tasks
    )]
    pub max_concurrent_mempool_tasks: usize,

    /// Maximum number of concurrent handlers for chunk ingress messages
    #[arg(skip)]
    pub max_concurrent_chunk_ingress_tasks: usize,

    /// Backpressure channel capacity for the async chunk write-behind buffer.
    /// Controls how many chunk writes can be queued before the sender blocks.
    #[arg(
        long = "mempool.chunk-writer-buffer-size",
        default_value_t = Self::default().chunk_writer_buffer_size
    )]
    pub chunk_writer_buffer_size: usize,
}

impl Default for MempoolNodeConfig {
    fn default() -> Self {
        Self {
            max_pending_pledge_items: 1000,
            max_pledges_per_item: 10,
            max_pending_chunk_items: 500,
            max_chunks_per_item: 100,
            max_preheader_chunks_per_item: 50,
            max_preheader_data_path_bytes: 4096,
            max_valid_items: 10_000,
            max_invalid_items: 5_000,
            max_valid_chunks: 5_000,
            max_valid_submit_txs: 3_000,
            max_valid_commitment_addresses: 1_000,
            max_commitments_per_address: 5,
            max_concurrent_mempool_tasks: 30,
            max_concurrent_chunk_ingress_tasks: 30,
            chunk_writer_buffer_size: 4096,
        }
    }
}

impl NodeConfig {
    pub fn consensus_config(&self) -> ConsensusConfig {
        // load the consensus config
        // todo: lazy load the consensus config, caching the result for subsequent calls

        match &self.consensus {
            ConsensusOptions::Path(path_buf) => std::fs::read_to_string(path_buf)
                .map(|consensus_cfg| {
                    toml::from_str::<ConsensusConfig>(&consensus_cfg)
                        .expect("invalid consensus file")
                })
                .expect("consensus cfg does not exist"),
            ConsensusOptions::Testnet => ConsensusConfig::testnet(),
            ConsensusOptions::Testing => ConsensusConfig::testing(),
            ConsensusOptions::Mainnet => ConsensusConfig::mainnet(),
            ConsensusOptions::Custom(consensus_config) => consensus_config.clone(),
        }
    }

    pub fn with_consensus<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ConsensusConfig),
    {
        f(self.consensus.get_mut());
        self
    }

    pub fn with_genesis_peer_discovery_timeout(mut self, timeout_millis: u64) -> Self {
        self.genesis_peer_discovery_timeout_millis = timeout_millis;
        self
    }

    pub fn miner_address(&self) -> IrysAddress {
        IrysAddress::from_private_key(&self.mining_key)
    }

    pub fn new_random_signer(&self) -> IrysSigner {
        IrysSigner::random_signer(&self.consensus_config())
    }

    pub fn signer(&self) -> IrysSigner {
        IrysSigner {
            signer: self.mining_key.clone(),
            chain_id: self.consensus_config().chain_id,
            chunk_size: self.consensus_config().chunk_size,
        }
    }

    pub fn local_api_url(&self) -> String {
        format!(
            "http://{}:{}",
            self.http.bind_ip(&self.network_defaults),
            self.http.bind_port
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn fund_genesis_accounts<'a>(
        &mut self,
        signers: impl IntoIterator<Item = &'a IrysSigner>,
    ) -> &mut Self {
        let mut accounts: Vec<(IrysAddress, GenesisAccount)> = Vec::new();
        for signer in signers {
            accounts.push((
                signer.address(),
                GenesisAccount {
                    balance: alloy_primitives::U256::from(99999000000000000000000_u128),
                    ..Default::default()
                },
            ))
        }
        self.consensus.extend_genesis_accounts(accounts);
        self
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing_with_signer(signer: &IrysSigner) -> Self {
        let mining_key = signer.signer.clone();
        let reward_address = signer.address();
        let mut consensus = ConsensusConfig::testing();
        consensus.genesis.miner_address = reward_address;
        consensus.genesis.reward_address = reward_address;
        Self {
            node_mode: NodeMode::Genesis,
            sync_mode: SyncMode::Full,
            consensus: ConsensusOptions::Custom(consensus),
            base_directory: default_irys_path(),

            oracles: vec![OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::token(dec!(0.00000000000001))
                    .expect("valid token amount"),
                smoothing_interval: 15,
                initial_direction_up: true,
                poll_interval_ms: default_mock_oracle_poll_interval_ms(),
            }],
            mining_key,
            reward_address,
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            data_sync: DataSyncServiceConfig {
                max_pending_chunk_requests: 1000,
                max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
                bandwidth_adjustment_interval: Duration::from_secs(5),
                chunk_request_timeout: Duration::from_secs(10),
            },
            trusted_peers: vec![/* PeerAddress {
                api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
                gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
                execution: crate::RethPeerInfo::default(), // TODO: figure out how to pre-compute peer IDs
            }*/],
            initial_stake_and_pledge_whitelist: vec![],
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            network_defaults: NetworkDefaults {
                public_ip: "127.0.0.1".to_string(),
                bind_ip: "127.0.0.1".to_string(),
            },
            gossip: GossipConfig {
                public_ip: None,
                public_port: 0,
                bind_ip: None,
                bind_port: 0,
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: true,
                    public_ip: Some("0.0.0.0".to_string()),
                    public_port: 0,
                    bind_ip: Some("0.0.0.0".to_string()),
                    bind_port: 0,
                    peer_id: Default::default(),
                },
                rpc: Default::default(),
                txpool: Default::default(),
                engine: Default::default(),
                metrics: Default::default(),
            },
            packing: PackingConfig {
                local: LocalPackingConfig {
                    cpu_packing_concurrency: 4,
                    gpu_packing_batch_size: 1024,
                },
                remote: Default::default(),
            },
            cache: CacheConfig {
                cache_clean_lag: 2,
                max_cache_size_bytes: CacheConfig::default().max_cache_size_bytes,
                prune_at_capacity_percent: CacheConfig::default().prune_at_capacity_percent,
            },
            http: HttpConfig {
                public_ip: None,
                public_port: 0,
                bind_ip: None,
                bind_port: 0,
            },
            mempool: MempoolNodeConfig {
                max_pending_pledge_items: 100,
                max_pledges_per_item: 100,
                max_pending_chunk_items: 30,
                max_chunks_per_item: 500,
                max_preheader_chunks_per_item: 64,
                max_preheader_data_path_bytes: 64 * 1024,
                max_invalid_items: 10_000,
                max_valid_items: 10_000,
                max_valid_chunks: 10_000,
                max_valid_submit_txs: 3000,
                max_valid_commitment_addresses: 300,
                max_commitments_per_address: 20,
                max_concurrent_mempool_tasks: 30,
                max_concurrent_chunk_ingress_tasks: 30,
                chunk_writer_buffer_size: 4096,
            },

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: 4,
            },

            p2p_handshake: P2PHandshakeConfig::default(),
            p2p_gossip: P2PGossipConfig::default(),
            p2p_pull: P2PPullConfig::default(),
            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,
            sync: SyncConfig::default(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing_with_epochs(num_blocks_in_epoch: usize) -> Self {
        let mut node_config = Self::testing();
        node_config.consensus.get_mut().epoch.num_blocks_in_epoch = num_blocks_in_epoch as u64;
        node_config
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing() -> Self {
        use k256::ecdsa::SigningKey;
        let mining_key = SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key");
        let signer = IrysSigner {
            signer: mining_key,
            chain_id: 0,
            chunk_size: 0,
        };

        Self::testing_with_signer(&signer)
    }

    pub fn testnet() -> Self {
        use k256::ecdsa::SigningKey;
        let mining_key = SigningKey::from_slice(
            &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                .expect("valid hex"),
        )
        .expect("valid key");
        let mut consensus = ConsensusConfig::testnet();
        let signer = IrysSigner {
            signer: mining_key,
            chain_id: consensus.chain_id,
            chunk_size: consensus.chunk_size,
        };

        let mining_key = signer.signer.clone();
        let reward_address = signer.address();
        consensus.genesis.miner_address = reward_address;
        consensus.genesis.reward_address = reward_address;
        consensus.expected_genesis_hash = Some(H256::zero());
        Self {
            node_mode: NodeMode::Peer,
            sync_mode: SyncMode::Full,
            consensus: ConsensusOptions::Custom(consensus),
            base_directory: default_irys_path(),

            oracles: vec![OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::token(dec!(0.00000000000001))
                    .expect("valid token amount"),
                smoothing_interval: 15,
                initial_direction_up: true,
                poll_interval_ms: default_mock_oracle_poll_interval_ms(),
            }],
            mining_key,
            reward_address,
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            data_sync: DataSyncServiceConfig {
                max_pending_chunk_requests: 1000,
                max_storage_throughput_bps: 200 * 1024 * 1024, // 200 MB/s
                bandwidth_adjustment_interval: Duration::from_secs(5),
                chunk_request_timeout: Duration::from_secs(10),
            },
            trusted_peers: vec![],
            // trusted_peers: vec![PeerAddress {
            //     api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
            //     gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
            //     execution: reth_peer_info, // TODO: figure out how to pre-compute peer IDs
            // }],
            initial_stake_and_pledge_whitelist: vec![],
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            network_defaults: NetworkDefaults {
                public_ip: "127.0.0.1".to_string(),
                bind_ip: "0.0.0.0".to_string(),
            },
            gossip: GossipConfig {
                public_ip: None,
                public_port: 8081,
                bind_ip: None,
                bind_port: 8081,
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: false,
                    public_ip: None,
                    public_port: 9009,
                    bind_ip: Some("127.0.0.1".to_string()),
                    bind_port: 9009,
                    peer_id: Default::default(),
                },
                rpc: Default::default(),
                txpool: Default::default(),
                engine: Default::default(),
                metrics: Default::default(),
            },
            packing: PackingConfig {
                local: LocalPackingConfig {
                    cpu_packing_concurrency: 4,
                    gpu_packing_batch_size: 1024,
                },
                remote: Default::default(),
            },
            cache: CacheConfig {
                cache_clean_lag: 2,
                max_cache_size_bytes: CacheConfig::default().max_cache_size_bytes,
                prune_at_capacity_percent: CacheConfig::default().prune_at_capacity_percent,
            },
            http: HttpConfig {
                public_ip: None,
                public_port: 8080,
                bind_ip: None,
                bind_port: 8080,
            },

            mempool: MempoolNodeConfig {
                max_pending_pledge_items: 100,
                max_pledges_per_item: 100,
                max_pending_chunk_items: 30,
                max_chunks_per_item: 500,
                max_preheader_chunks_per_item: 64,
                max_preheader_data_path_bytes: 64 * 1024,
                max_invalid_items: 10_000,
                max_valid_items: 10_000,
                max_valid_chunks: 10_000,
                max_valid_submit_txs: 3000,
                max_valid_commitment_addresses: 300,
                max_commitments_per_address: 20,
                max_concurrent_mempool_tasks: 30,
                max_concurrent_chunk_ingress_tasks: 30,
                chunk_writer_buffer_size: 4096,
            },

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: 4,
            },

            p2p_handshake: P2PHandshakeConfig::default(),
            p2p_gossip: P2PGossipConfig::default(),
            p2p_pull: P2PPullConfig::default(),

            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,

            sync: SyncConfig::default(),
        }
    }

    /// get the storage module directory path
    pub fn storage_module_dir(&self) -> PathBuf {
        self.base_directory.join("storage_modules")
    }
    /// get the irys consensus data directory path
    pub fn irys_consensus_data_dir(&self) -> PathBuf {
        self.base_directory.join("irys_consensus_data")
    }

    /// get the irys mempool persistence path
    pub fn mempool_dir(&self) -> PathBuf {
        self.base_directory.join("mempool")
    }
    /// get the reth data directory path
    pub fn reth_data_dir(&self) -> PathBuf {
        self.base_directory.join("reth")
    }
    /// get the reth log directory path
    pub fn reth_log_dir(&self) -> PathBuf {
        self.reth_data_dir().join("logs")
    }
    /// get the `block_index` directory path
    pub fn block_index_dir(&self) -> PathBuf {
        self.base_directory.join("block_index")
    }

    /// get the `vdf_steps` directory path
    pub fn vdf_steps_dir(&self) -> PathBuf {
        self.base_directory.join("vdf_steps")
    }

    /// get the peer info directory path
    pub fn peer_info_dir(&self) -> PathBuf {
        self.base_directory.join("peer_info")
    }

    /// Get the PeerAddress for this node configuration
    pub fn peer_address(&self) -> PeerAddress {
        PeerAddress {
            api: format!(
                "{}:{}",
                self.http.public_ip(&self.network_defaults),
                self.http.public_port
            )
            .parse()
            .expect("valid SocketAddr expected"),
            gossip: format!(
                "{}:{}",
                self.gossip.public_ip(&self.network_defaults),
                self.gossip.public_port
            )
            .parse()
            .expect("valid SocketAddr expected"),
            execution: RethPeerInfo {
                peering_tcp_addr: format!(
                    "{}:{}",
                    self.reth.network.public_ip(&self.network_defaults),
                    self.reth.network.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                peer_id: self.reth.network.peer_id,
            },
        }
    }

    /// Check if the node should only interact with trusted peers
    pub fn is_trusted_peers_only(&self) -> bool {
        matches!(self.peer_filter_mode, PeerFilterMode::TrustedOnly)
    }

    /// Check if the node should interact with trusted peers and their handshake peers
    pub fn is_trusted_and_handshake_mode(&self) -> bool {
        matches!(self.peer_filter_mode, PeerFilterMode::TrustedAndHandshake)
    }

    /// Check if the node has peer filtering enabled (not unrestricted)
    pub fn has_peer_filtering(&self) -> bool {
        !matches!(self.peer_filter_mode, PeerFilterMode::Unrestricted)
    }

    pub fn vdf(&self) -> VdfConfig {
        self.into()
    }

    pub fn mempool(&self) -> MempoolConfig {
        self.into()
    }
}

fn default_irys_path() -> PathBuf {
    env::current_dir()
        .expect("Unable to determine working dir, aborting")
        .join(".irys")
}
