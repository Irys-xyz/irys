use crate::{
    irys::IrysSigner,
    storage_pricing::{
        phantoms::{IrysPrice, Usd},
        Amount,
    },
    ConsensusConfig, MempoolConfig, PeerAddress, RethPeerInfo, VdfConfig,
};
use crate::{serde_utils, ConsensusOptions};
use alloy_genesis::GenesisAccount;
use alloy_primitives::Address;

use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::{env, path::PathBuf, time::Duration};

/// # Node Configuration
///
/// The main configuration for an Irys node, containing all settings needed
/// to participate in the network. This includes network mode, consensus rules,
/// pricing parameters, and system resource allocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Initial whitelist of peers to connect to. If you're joining the network as a peer in a
    /// trusted-only or trusted-and-handshake mode, you'll be supplied one during the handshake
    /// with the trusted peers. For the original trusted peer that has to be set.
    #[serde(default)]
    pub initial_whitelist: Vec<SocketAddr>,

    /// Controls how the node filters peer interactions
    #[serde(default = "default_peer_filter_mode")]
    pub peer_filter_mode: PeerFilterMode,

    pub reward_address: Address,

    // whether we should try to stake & pledge our local drives
    pub stake_pledge_drives: bool,

    pub genesis_peer_discovery_timeout_millis: u64,

    /// Peer-to-peer network communication settings
    pub gossip: GossipConfig,

    /// HTTP API server configuration
    pub http: HttpConfig,

    /// Reth node configuration
    pub reth: RethConfig,

    /// StorageModule configuration
    pub storage: StorageSyncConfig,

    /// DataSyncService configuration
    pub data_sync: DataSyncServiceConfig,

    /// Data packing and compression settings
    pub packing: PackingConfig,

    /// Cache management configuration
    pub cache: CacheConfig,

    /// Settings for the price oracle system
    pub oracle: OracleConfig,

    pub vdf: VdfNodeConfig,

    pub mempool: MempoolNodeConfig,

    /// Specifies which consensus rules the node follows
    pub consensus: ConsensusOptions,

    /// P2P handshake parameters
    #[serde(default)]
    pub p2p_handshake: P2PHandshakeConfig,
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

    /// Join an existing network by connecting to trusted peers
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
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum OracleConfig {
    /// A simulated price oracle for testing and development
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
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StorageSyncConfig {
    /// Number of write operations before forcing a sync to disk
    /// Higher values improve performance but increase data loss risk on crashes
    pub num_writes_before_sync: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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

/// # Gossip Network Configuration
///
/// Settings for peer-to-peer communication between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GossipConfig {
    /// The IP address that's going to be announced to other peers
    pub public_ip: String,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address the gossip service binds to
    pub bind_ip: String,
    /// The port number the gossip service listens on
    pub bind_port: u16,
}

/// # Reth Node Configuration
///
/// Settings that are passed to the reth node
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethConfig {
    pub network: RethNetworkConfig,
}

/// # Reth network Configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethNetworkConfig {
    #[serde(default)]
    pub use_random_ports: bool,
    /// The IP address that's going to be announced to other peers
    pub public_ip: String,
    /// The port to accept connections from other peers
    pub public_port: u16,
    /// The IP address that Reth binds to
    pub bind_ip: String,
    /// The port number the Reth listens on
    pub bind_port: u16,
    // peer ID
    // WARNING: this gets overridden partway through the startup sequence with the correct value
    #[serde(default)]
    pub peer_id: reth_transaction_pool::PeerId,
}

/// # Data Packing Configuration
///
/// Controls how data is compressed and packed for storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackingConfig {
    /// Number of CPU threads to use for data packing operations
    pub cpu_packing_concurrency: u16,

    /// Batch size for GPU-accelerated packing operations
    pub gpu_packing_batch_size: u32,
}

/// # Cache Configuration
///
/// Settings for in-memory caching to improve performance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Number of blocks cache cleaning will lag behind block finalization
    /// Higher values keep more data in cache but use more memory
    pub cache_clean_lag: u8,
}

/// # HTTP API Configuration
///
/// Settings for the node's HTTP server that provides API access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// The IP address visible to the outside world
    pub public_ip: String,
    /// The port that is visible to the outside world
    pub public_port: u16,
    /// The IP address the HTTP service binds to
    pub bind_ip: String,
    /// The port that the Node's HTTP server should listen on. Set to 0 for randomization.
    pub bind_port: u16,
}

/// P2P handshake configuration with sensible defaults
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct P2PHandshakeConfig {
    pub max_concurrent_handshakes: usize,
    pub max_peers_per_response: usize,
    pub max_retries: u32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
    pub blocklist_ttl_secs: u64,
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

/// Default for `peer_filter_mode` when the field is not present in the provided TOML.
/// This keeps legacy configurations working by defaulting to unrestricted mode.
fn default_peer_filter_mode() -> PeerFilterMode {
    PeerFilterMode::Unrestricted
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VdfNodeConfig {
    /// Maximum number of threads to use for parallel VDF verification
    pub parallel_verification_thread_limit: usize,
}

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MempoolNodeConfig {
    /// Maximum number of addresses in the LRU cache for out-of-order stakes and pledges
    /// Controls memory usage for tracking transactions that arrive before their dependencies
    pub max_pending_pledge_items: usize,

    /// Maximum number of pending pledge transactions allowed per address
    /// Limits the resources that can be consumed by a single address
    pub max_pledges_per_item: usize,

    /// Maximum number of transaction data roots to keep in the pending cache
    /// For transactions whose chunks arrive before the transaction header
    pub max_pending_chunk_items: usize,

    /// Maximum number of chunks that can be cached per data root
    /// Prevents memory exhaustion from excessive chunk storage for a single transaction
    pub max_chunks_per_item: usize,

    /// Maximum number of pre-header chunks to keep per data root before the header arrives
    /// Limits speculative storage window for out-of-order chunks
    #[serde(default)]
    pub max_preheader_chunks_per_item: usize,

    /// Maximum allowed pre-header data_path bytes for chunk proofs
    /// Mitigates DoS on speculative chunk storage before header arrival
    #[serde(default)]
    pub max_preheader_data_path_bytes: usize,

    /// Maximum number of valid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_valid_items: usize,

    /// Maximum number of invalid tx txids to keep track of
    /// Decreasing this will increase the amount of validation the node will have to perform
    pub max_invalid_items: usize,

    /// Maximum number of valid chunk hashes to keep track of
    /// Prevents re-processing and re-gossipping of recently seen chunks
    pub max_valid_chunks: usize,
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
            ConsensusOptions::Custom(consensus_config) => consensus_config.clone(),
        }
    }

    pub fn miner_address(&self) -> Address {
        Address::from_private_key(&self.mining_key)
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
        format!("http://{}:{}", self.http.bind_ip, self.http.bind_port)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn fund_genesis_accounts<'a>(
        &mut self,
        signers: impl IntoIterator<Item = &'a IrysSigner>,
    ) -> &mut Self {
        let mut accounts: Vec<(Address, GenesisAccount)> = Vec::new();
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

            oracle: OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::percentage(dec!(0.01)).expect("valid percentage"),
                smoothing_interval: 15,
            },
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
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            gossip: GossipConfig {
                public_ip: "127.0.0.1".parse().expect("valid IP address"),
                public_port: 0,
                bind_ip: "127.0.0.1".parse().expect("valid IP address"),
                bind_port: 0,
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: true,
                    public_ip: "0.0.0.0".parse().expect("valid IP address"),
                    public_port: 0,
                    bind_ip: "0.0.0.0".parse().expect("valid IP address"),
                    bind_port: 0,
                    peer_id: Default::default(),
                },
            },
            packing: PackingConfig {
                cpu_packing_concurrency: 4,
                gpu_packing_batch_size: 1024,
            },
            cache: CacheConfig { cache_clean_lag: 2 },
            http: HttpConfig {
                public_ip: "127.0.0.1".parse().expect("valid IP address"),
                public_port: 0,
                bind_ip: "127.0.0.1".parse().expect("valid IP address"),
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
            },

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: 4,
            },

            p2p_handshake: P2PHandshakeConfig::default(),
            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,
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
        Self {
            node_mode: NodeMode::Peer,
            sync_mode: SyncMode::Full,
            consensus: ConsensusOptions::Custom(consensus),
            base_directory: default_irys_path(),

            oracle: OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                incremental_change: Amount::percentage(dec!(0.01)).expect("valid percentage"),
                smoothing_interval: 15,
            },
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
            initial_whitelist: vec![],
            peer_filter_mode: PeerFilterMode::Unrestricted,
            gossip: GossipConfig {
                public_ip: "127.0.0.1".parse().expect("valid IP address"),
                public_port: 8081,
                bind_ip: "0.0.0.0".parse().expect("valid IP address"),
                bind_port: 8081,
            },
            reth: RethConfig {
                network: RethNetworkConfig {
                    use_random_ports: false,
                    public_ip: "127.0.0.1".parse().expect("valid IP address"),
                    public_port: 9009,
                    bind_ip: "127.0.0.1".parse().expect("valid IP address"),
                    bind_port: 9009,
                    peer_id: Default::default(),
                },
            },
            packing: PackingConfig {
                cpu_packing_concurrency: 4,
                gpu_packing_batch_size: 1024,
            },
            cache: CacheConfig { cache_clean_lag: 2 },
            http: HttpConfig {
                public_ip: "127.0.0.1".parse().expect("valid IP address"),
                public_port: 8080,
                bind_ip: "0.0.0.0".parse().expect("valid IP address"),
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
            },

            vdf: VdfNodeConfig {
                parallel_verification_thread_limit: 4,
            },

            p2p_handshake: P2PHandshakeConfig::default(),

            genesis_peer_discovery_timeout_millis: 10000,
            stake_pledge_drives: false,
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

    /// Get the PeerAddress for this node configuration
    pub fn peer_address(&self) -> PeerAddress {
        PeerAddress {
            api: format!("{}:{}", self.http.public_ip, self.http.public_port)
                .parse()
                .expect("valid SocketAddr expected"),
            gossip: format!("{}:{}", self.gossip.public_ip, self.gossip.public_port)
                .parse()
                .expect("valid SocketAddr expected"),
            execution: RethPeerInfo {
                peering_tcp_addr: format!(
                    "{}:{}",
                    &self.reth.network.public_ip, &self.reth.network.public_port
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
