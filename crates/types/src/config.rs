use crate::{
    irys::IrysSigner,
    storage_pricing::{
        phantoms::{CostPerGb, DecayRate, Irys, IrysPrice, Percentage, Usd},
        Amount,
    },
    PeerAddress, RethPeerInfo,
};
use alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::Address;
use reth_chainspec::Chain;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, ops::Deref, path::PathBuf, sync::Arc};

/// Ergonomic and cheaply copyable Configuration that has the consensus and user-defined configs extracted out
#[derive(Debug, Clone)]
pub struct Config(Arc<CombinedConfigInner>);

impl Config {
    pub fn new(node_config: NodeConfig) -> Self {
        let consensus = node_config.consensus_config();
        Self(Arc::new(CombinedConfigInner {
            consensus,
            node_config,
        }))
    }

    pub fn irys_signer(&self) -> IrysSigner {
        IrysSigner {
            signer: self.node_config.mining_key.clone(),
            chain_id: self.consensus.chain_id,
            chunk_size: self.consensus.chunk_size,
        }
    }
}

impl Deref for Config {
    type Target = CombinedConfigInner;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[derive(Debug)]
pub struct CombinedConfigInner {
    pub consensus: ConsensusConfig,
    pub node_config: NodeConfig,
}

/// # Consensus Configuration
///
/// Defines the core parameters that govern the Irys network consensus rules.
/// These parameters determine how the network operates, including pricing,
/// storage requirements, and data validation mechanisms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusConfig {
    /// Unique identifier for the blockchain network
    pub chain_id: u64,

    /// Reth chain spec for the reth genesis
    pub reth: RethChainSpec,

    /// Settings for the transaction memory pool
    pub mempool: MempoolConfig,

    /// Controls how mining difficulty adjusts over time
    pub difficulty_adjustment: DifficultyAdjustmentConfig,

    /// Defines the acceptable range of token price fluctuation between consecutive blocks
    /// This helps prevent price manipulation and ensures price stability
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub token_price_safe_range: Amount<Percentage>,

    /// The initial price of the Irys token at genesis in USD
    /// Sets the baseline for all future pricing calculations
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub genesis_price: Amount<(IrysPrice, Usd)>,

    /// The annual cost in USD for storing 1GB of data on the Irys network
    /// Used as the foundation for calculating storage fees
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub annual_cost_per_gb: Amount<(CostPerGb, Usd)>,

    /// Annual rate at which storage costs are expected to decrease
    /// Accounts for technological improvements making storage cheaper over time
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub decay_rate: Amount<DecayRate>,

    /// Configuration for the Verifiable Delay Function used in consensus
    pub vdf: VdfConfig,

    /// Configuration for block rewards
    pub block_reward_config: BlockRewardConfig,

    /// Size of each data chunk in bytes
    pub chunk_size: u64,

    /// Defines how many blocks must pass before a block is marked as finalized
    pub block_migration_depth: u32,

    /// Number of blocks to retain in the block tree from chain head
    pub block_tree_depth: u64,

    /// Number of chunks that make up a single partition
    pub num_chunks_in_partition: u64,

    /// Number of chunks that can be recalled in a single operation
    pub num_chunks_in_recall_range: u64,

    /// Number of partitions in each storage slot
    pub num_partitions_per_slot: u64,

    /// Number of iterations for entropy packing algorithm
    pub entropy_packing_iterations: u32,

    /// Cache management configuration
    pub epoch: EpochConfig,

    /// Configuration for Exponential Moving Average price calculations
    pub ema: EmaConfig,

    /// Minimum number of replicas required for data to be considered permanently stored
    /// Higher values increase data durability but require more network resources
    pub number_of_ingress_proofs: u64,

    /// Target number of years data should be preserved on the network
    /// Determines long-term storage pricing and incentives
    pub safe_minimum_number_of_years: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRewardConfig {
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub inflation_cap: Amount<Irys>,
    pub half_life_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RethChainSpec {
    /// The type of chain.
    pub chain: Chain,
    /// Genesis block.
    pub genesis: Genesis,
}

/// # Node Configuration
///
/// The main configuration for an Irys node, containing all settings needed
/// to participate in the network. This includes network mode, consensus rules,
/// pricing parameters, and system resource allocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Determines how the node joins and interacts with the network
    pub mode: NodeMode,

    /// The initial list of peers to contact for block sync
    pub trusted_peers: Vec<PeerAddress>,

    /// The base directory where to look for artifact data
    #[serde(default = "default_irys_path")]
    pub base_directory: PathBuf,

    /// Specifies which consensus rules the node follows
    pub consensus: ConsensusOptions,

    /// Settings for the price oracle system
    pub oracle: OracleConfig,

    /// Private key used for mining operations
    /// This key identifies the node and receives mining rewards
    #[serde(
        deserialize_with = "serde_utils::signing_key_from_hex",
        serialize_with = "serde_utils::serializes_signing_key"
    )]
    pub mining_key: k256::ecdsa::SigningKey,

    pub reward_address: Address,

    /// Data storage configuration
    pub storage: StorageSyncConfig,

    /// Fee and pricing settings
    pub pricing: PricingConfig,

    /// Peer-to-peer network communication settings
    pub gossip: GossipConfig,

    /// Data packing and compression settings
    pub packing: PackingConfig,

    /// Cache management configuration
    pub cache: CacheConfig,

    /// HTTP API server configuration
    pub http: HttpConfig,

    /// Reth node configuration
    pub reth: RethConfig,

    /// Reth settings
    pub reth_peer_info: RethPeerInfo,

    pub genesis_peer_discovery_timeout_millis: u64,
}

impl From<NodeConfig> for Config {
    fn from(val: NodeConfig) -> Self {
        Self::new(val)
    }
}

/// # Node Operation Mode
///
/// Defines how the node participates in the network - either as a genesis node
/// that starts a new network or as a peer that syncs with existing nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeMode {
    /// Start a new blockchain network as the first node
    Genesis,

    /// Join an existing network by connecting to trusted peers
    PeerSync,
}

/// # Consensus Configuration Source
///
/// Specifies where the node should obtain its consensus rules from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsensusOptions {
    /// Load consensus configuration from a file at the specified path
    Path(PathBuf),

    /// Use predefined testnet consensus parameters
    Testnet,

    /// Use custom consensus parameters defined elsewhere
    Custom(ConsensusConfig),
}

impl ConsensusOptions {
    pub fn extend_genesis_accounts(
        &mut self,
        accounts: impl IntoIterator<Item = (Address, GenesisAccount)>,
    ) {
        let config = self.get_mut();
        config.reth.genesis = config.reth.genesis.clone().extend_accounts(accounts);
    }

    pub fn get_mut(&mut self) -> &mut ConsensusConfig {
        let Self::Custom(config) = self else {
            panic!("only support mutating custom configs");
        };

        config
    }
}

/// # Pricing Configuration
///
/// Controls how the node calculates fees for storage and other services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingConfig {
    /// Additional fee percentage added by nodes to the base storage cost
    /// This provides an incentive for nodes to participate in the network
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub fee_percentage: Amount<Percentage>,
}

/// # Oracle Configuration
///
/// Defines how the node obtains and processes external price information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
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
            deserialize_with = "serde_utils::percentage_amount",
            serialize_with = "serde_utils::serializes_percentage_amount"
        )]
        percent_change: Amount<Percentage>,

        /// Number of blocks between price updates
        smoothing_interval: u64,
    },
}

/// # EMA (Exponential Moving Average) Configuration
///
/// Controls how token prices are smoothed over time to reduce volatility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmaConfig {
    /// Number of blocks between EMA price recalculations
    /// Lower values make prices more responsive, higher values provide more stability
    pub price_adjustment_interval: u64,
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdfConfig {
    /// VDF reset frequency in global steps
    /// Formula: blocks_between_resets × vdf_steps_per_block
    /// Example: 50 blocks × 12 steps = 600 global steps
    /// At 12s/block target, resets occur every ~10 minutes
    pub reset_frequency: usize,

    /// Maximum number of threads to use for parallel VDF verification
    pub parallel_verification_thread_limit: usize,

    /// Number of checkpoints to include in each VDF step
    pub num_checkpoints_in_vdf_step: usize,

    /// Minimum number of steps to store in FIFO VecDeque to allow for network forks
    pub max_allowed_vdf_fork_steps: u64,

    /// Target number of SHA-1 operations per second for VDF calibration
    pub sha_1s_difficulty: u64,
}

impl VdfConfig {
    /// Returns the number of iterations per checkpoint,
    /// computed as the floor of (step difficulty ÷ number of checkpoints in a step).
    pub fn num_iterations_per_checkpoint(&self) -> u64 {
        self.sha_1s_difficulty / self.num_checkpoints_in_vdf_step as u64
    }
}

/// # Epoch Configuration
///
/// Controls the timing and parameters for network epochs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EpochConfig {
    /// Scaling factor for the capacity projection curve
    /// Affects how network capacity is calculated and projected
    pub capacity_scalar: u64,

    /// Number of blocks in a single epoch
    pub num_blocks_in_epoch: u64,

    /// Number of epochs between ledger submissions
    pub submit_ledger_epoch_length: u64,

    /// Optional configuration for capacity partitioning
    pub num_capacity_partitions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StorageSyncConfig {
    /// Number of write operations before forcing a sync to disk
    /// Higher values improve performance but increase data loss risk on crashes
    pub num_writes_before_sync: u64,
}

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolConfig {
    /// Maximum number of data transactions that can be included in a single block
    pub max_data_txs_per_block: u64,

    /// The number of blocks a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub anchor_expiry_depth: u8,

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
}

/// # Gossip Network Configuration
///
/// Settings for peer-to-peer communication between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// # Reth Configuration
///
/// Settings that are passed to the reth node
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RethConfig {
    pub use_random_ports: bool,
}

/// # Data Packing Configuration
///
/// Controls how data is compressed and packed for storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct CacheConfig {
    /// Number of blocks cache cleaning will lag behind block finalization
    /// Higher values keep more data in cache but use more memory
    pub cache_clean_lag: u8,
}

/// # HTTP API Configuration
///
/// Settings for the node's HTTP server that provides API access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// # Difficulty Adjustment Configuration
///
/// Controls how mining difficulty changes over time to maintain target block times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifficultyAdjustmentConfig {
    /// Target time between blocks in seconds
    pub block_time: u64,

    /// Number of blocks between difficulty adjustments
    pub difficulty_adjustment_interval: u64,

    /// Maximum factor by which difficulty can increase in a single adjustment
    pub max_difficulty_adjustment_factor: Decimal,

    /// Minimum factor by which difficulty can decrease in a single adjustment
    pub min_difficulty_adjustment_factor: Decimal,
}

fn default_irys_path() -> PathBuf {
    env::current_dir()
        .expect("Unable to determine working dir, aborting")
        .join(".irys")
}

impl ConsensusConfig {
    // This is hardcoded here to be used just by C packing related stuff as it is also hardcoded right now in C sources
    // TODO: get rid of this hardcoded variable? Otherwise altering the `chunk_size` in the configs may have
    // discrepancies when using GPU mining
    pub const CHUNK_SIZE: u64 = 256 * 1024;

    pub fn testnet() -> Self {
        const DEFAULT_BLOCK_TIME: u64 = 1;
        const IRYS_TESTNET_CHAIN_ID: u64 = 1270;

        // block reward params
        const HALF_LIFE_YEARS: u128 = 4;
        const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;
        const INFLATION_CAP: u128 = 100_000_000;

        Self {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(), // 0.01$
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),    // 1%
            safe_minimum_number_of_years: 200,
            number_of_ingress_proofs: 10,
            genesis_price: Amount::token(dec!(1)).expect("valid token amount"),
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            mempool: MempoolConfig {
                max_data_txs_per_block: 100,
                anchor_expiry_depth: 10,
                // TODO: Move the following to a node config
                max_pending_pledge_items: 100,
                max_pledges_per_item: 100,
                max_pending_chunk_items: 30,
                max_chunks_per_item: 500,
            },
            vdf: VdfConfig {
                // Reset VDF every ~50 blocks (50 blocks × 12 steps/block = 600 global steps)
                // With 12s target block time, this resets approximately every 10 minutes
                reset_frequency: 50 * 12,
                parallel_verification_thread_limit: 4,
                num_checkpoints_in_vdf_step: 25,
                max_allowed_vdf_fork_steps: 60_000,
                sha_1s_difficulty: 70_000,
            },
            chunk_size: Self::CHUNK_SIZE,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            block_migration_depth: 6,
            block_tree_depth: 50,
            epoch: EpochConfig {
                capacity_scalar: 100,
                num_blocks_in_epoch: 100,
                submit_ledger_epoch_length: 5,
                num_capacity_partitions: None,
            },
            entropy_packing_iterations: 1000,
            difficulty_adjustment: DifficultyAdjustmentConfig {
                block_time: DEFAULT_BLOCK_TIME,
                difficulty_adjustment_interval: (24_u64 * 60 * 60 * 1000)
                    .div_ceil(DEFAULT_BLOCK_TIME)
                    * 14,
                max_difficulty_adjustment_factor: dec!(4),
                min_difficulty_adjustment_factor: dec!(0.25),
            },
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            reth: RethChainSpec {
                chain: Chain::from_id(IRYS_TESTNET_CHAIN_ID),
                genesis: Genesis {
                    gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
                    alloc: {
                        let mut map = BTreeMap::new();
                        map.insert(
                            Address::from_slice(
                                hex::decode("64f1a2829e0e698c18e7792d6e74f67d89aa0a32")
                                    .unwrap()
                                    .as_slice(),
                            ),
                            GenesisAccount {
                                balance: alloy_primitives::U256::from(690000000000000000_u128),
                                ..Default::default()
                            },
                        );
                        map.insert(
                            Address::from_slice(
                                hex::decode("A93225CBf141438629f1bd906A31a1c5401CE924")
                                    .unwrap()
                                    .as_slice(),
                            ),
                            GenesisAccount {
                                balance: alloy_primitives::U256::from(
                                    1_000_000_000_000_000_000_000_000_u128,
                                ),
                                ..Default::default()
                            },
                        );
                        map
                    },
                    ..Default::default()
                },
            },
            block_reward_config: BlockRewardConfig {
                inflation_cap: Amount::token(rust_decimal::Decimal::from(INFLATION_CAP)).unwrap(),
                half_life_secs: (HALF_LIFE_YEARS * SECS_PER_YEAR).try_into().unwrap(),
            },
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

    pub fn api_uri(&self) -> String {
        format!("http://{}:{}", self.http.public_ip, self.http.public_port)
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
                    balance: alloy_primitives::U256::from(690000000000000000_u128),
                    ..Default::default()
                },
            ))
        }
        self.consensus.extend_genesis_accounts(accounts);
        self
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet_with_signer(signer: &IrysSigner) -> Self {
        let mining_key = signer.signer.clone();
        let reward_address = signer.address();
        Self {
            mode: NodeMode::Genesis,
            consensus: ConsensusOptions::Custom(ConsensusConfig::testnet()),
            base_directory: default_irys_path(),

            oracle: OracleConfig::Mock {
                initial_price: Amount::token(dec!(1)).expect("valid token amount"),
                percent_change: Amount::percentage(dec!(0.01)).expect("valid percentage"),
                smoothing_interval: 15,
            },
            mining_key,
            reward_address,
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            trusted_peers: vec![PeerAddress {
                api: "127.0.0.1:8080".parse().expect("valid SocketAddr expected"),
                gossip: "127.0.0.1:8081".parse().expect("valid SocketAddr expected"),
                execution: crate::RethPeerInfo::default(), // TODO: figure out how to pre-compute peer IDs
            }],
            pricing: PricingConfig {
                fee_percentage: Amount::percentage(dec!(0.01)).expect("valid percentage"),
            },
            gossip: GossipConfig {
                public_ip: "127.0.0.1".parse().expect("valid IP address"),
                public_port: 0,
                bind_ip: "127.0.0.1".parse().expect("valid IP address"),
                bind_port: 0,
            },
            reth: RethConfig {
                use_random_ports: true,
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
            reth_peer_info: RethPeerInfo::default(),

            genesis_peer_discovery_timeout_millis: 10000,
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet_with_epochs(num_blocks_in_epoch: usize) -> Self {
        let mut node_config = Self::testnet();
        node_config.consensus.get_mut().epoch.num_blocks_in_epoch = num_blocks_in_epoch as u64;
        node_config
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn testnet() -> Self {
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

        Self::testnet_with_signer(&signer)
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
            execution: self.reth_peer_info,
        }
    }
}

pub mod serde_utils {

    use rust_decimal::Decimal;
    use serde::{Deserialize as _, Deserializer, Serializer};

    use crate::storage_pricing::Amount;

    /// deserialize the token amount from a float.
    /// The float is expected to be in a format of 1.42.
    pub fn token_amount<'de, T: std::fmt::Debug, D>(deserializer: D) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        amount_from_float(deserializer, |dec| Amount::<T>::token(dec))
    }

    /// deserialize the percentage amount from a string.
    ///
    /// The string is expected to be:
    /// - 0.1 (10%)
    /// - 1.0 (100%)
    pub fn percentage_amount<'de, T: std::fmt::Debug, D>(
        deserializer: D,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        amount_from_float(deserializer, |dec| Amount::<T>::percentage(dec))
    }

    fn amount_from_float<'de, T: std::fmt::Debug, D>(
        deserializer: D,
        dec_to_amount: impl Fn(Decimal) -> eyre::Result<Amount<T>>,
    ) -> Result<Amount<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw_float = f64::deserialize(deserializer)?;
        let decimal = Decimal::try_from(raw_float).map_err(serde::de::Error::custom)?;
        let amount = dec_to_amount(decimal).map_err(serde::de::Error::custom)?;
        Ok(amount)
    }

    /// Deserialize a secp256k1 private key from a hex encoded string slice
    pub fn signing_key_from_hex<'de, D>(
        deserializer: D,
    ) -> Result<k256::ecdsa::SigningKey, D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = String::deserialize(deserializer)?;
        let decoded = hex::decode(bytes.as_bytes()).map_err(serde::de::Error::custom)?;
        let key =
            k256::ecdsa::SigningKey::from_slice(&decoded).map_err(serde::de::Error::custom)?;
        Ok(key)
    }

    pub fn serializes_signing_key<S>(
        key: &k256::ecdsa::SigningKey,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Convert to bytes and then hex-encode
        let key_bytes = key.to_bytes();
        let hex_string = hex::encode(key_bytes);
        serializer.serialize_str(&hex_string)
    }

    pub fn serializes_token_amount<S, T>(
        amount: &Amount<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: std::fmt::Debug,
    {
        // Convert to bytes and then hex-encode
        let decimal = amount
            .token_to_decimal()
            .map_err(serde::ser::Error::custom)?;
        let float: f64 = decimal
            .try_into()
            .expect("decimal to be convertible to a f64");
        serializer.serialize_f64(float)
    }

    pub fn serializes_percentage_amount<S, T>(
        amount: &Amount<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: std::fmt::Debug,
    {
        // Convert to bytes and then hex-encode
        let decimal = amount
            .percentage_to_decimal()
            .map_err(serde::ser::Error::custom)?;
        let float: f64 = decimal
            .try_into()
            .expect("decimal to be convertible to a f64");
        serializer.serialize_f64(float)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use toml;

    #[test]
    fn test_deserialize_consensus_config_from_toml() {
        let toml_data = r#"
        chain_id = 1270
        token_price_safe_range = 1.0
        genesis_price = 1.0
        annual_cost_per_gb = 0.01
        decay_rate = 0.01
        chunk_size = 262144
        block_migration_depth = 6
        block_tree_depth = 50
        num_chunks_in_partition = 10
        num_chunks_in_recall_range = 2
        num_partitions_per_slot = 1
        entropy_packing_iterations = 1000
        number_of_ingress_proofs = 10
        safe_minimum_number_of_years = 200
        genesis_peer_discovery_timeout_millis = 10000

        [reth]
        chain = 1270

        [reth.genesis]
        nonce = "0x0"
        timestamp = "0x0"
        extraData = "0x"
        gasLimit = "0x1c9c380"
        difficulty = "0x0"
        mixHash = "0x0000000000000000000000000000000000000000000000000000000000000000"
        coinbase = "0x0000000000000000000000000000000000000000"

        [reth.genesis.config]
        chainId = 1
        daoForkSupport = false
        terminalTotalDifficultyPassed = false

        [reth.genesis.alloc.0x64f1a2829e0e698c18e7792d6e74f67d89aa0a32]
        balance = "0x9935f581f050000"

        [reth.genesis.alloc.0xa93225cbf141438629f1bd906a31a1c5401ce924]
        balance = "0xd3c21bcecceda1000000"

        [mempool]
        max_data_txs_per_block = 100
        anchor_expiry_depth = 10
        max_pending_pledge_items = 100
        max_pledges_per_item = 100
        max_pending_chunk_items = 30
        max_chunks_per_item = 500

        [difficulty_adjustment]
        block_time = 1
        difficulty_adjustment_interval = 1209600000
        max_difficulty_adjustment_factor = 4
        min_difficulty_adjustment_factor = 0.25

        [vdf]
        reset_frequency = 600
        max_allowed_vdf_fork_steps = 60000
        parallel_verification_thread_limit = 4
        num_checkpoints_in_vdf_step = 25

        sha_1s_difficulty = 70000

        [block_reward_config]
        inflation_cap = 100000000
        half_life_secs = 126144000

        [epoch]
        capacity_scalar = 100
        num_blocks_in_epoch = 100
        submit_ledger_epoch_length = 5

        [ema]
        price_adjustment_interval = 10
        "#;

        // Create the expected config
        let expected_config = ConsensusConfig::testnet();
        let expected_toml_data = toml::to_string(&expected_config).unwrap();
        // for debugging purposes
        println!("{}", expected_toml_data);

        // Deserialize the TOML string into a ConsensusConfig
        let config = toml::from_str::<ConsensusConfig>(toml_data)
            .expect("Failed to deserialize ConsensusConfig from TOML");

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }

    #[test]
    fn test_deserialize_config_from_toml() {
        let toml_data = r#"
        mode = "Genesis"
        base_directory = "~/.tmp/.irys"
        consensus = "Testnet"
        mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
        reward_address = "0x64f1a2829e0e698c18e7792d6e74f67d89aa0a32"
        genesis_peer_discovery_timeout_millis = 10000

        [[trusted_peers]]
        gossip = "127.0.0.1:8081"
        api = "127.0.0.1:8080"

        [trusted_peers.execution]
        peering_tcp_addr = "127.0.0.1:30303"
        peer_id = "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"

        [oracle]
        type = "mock"
        initial_price = 1.0
        percent_change = 0.01
        smoothing_interval = 15

        [storage]
        num_writes_before_sync = 1

        [pricing]
        fee_percentage = 0.01

        [gossip]
        bind_ip = "127.0.0.1"
        bind_port = 0
        public_ip = "127.0.0.1"
        public_port = 0

        [packing]
        cpu_packing_concurrency = 4
        gpu_packing_batch_size = 1024

        [cache]
        cache_clean_lag = 2

        [http]
        bind_ip = "127.0.0.1"
        bind_port = 0
        public_ip = "127.0.0.1"
        public_port = 0

        [reth]
        use_random_ports = true

        [reth_peer_info]
        peering_tcp_addr = "0.0.0.0:0"
        peer_id = "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        "#;
        // Create the expected config
        let mut expected_config = NodeConfig::testnet();
        expected_config.consensus = ConsensusOptions::Testnet;
        expected_config.base_directory = PathBuf::from("~/.tmp/.irys");
        expected_config.trusted_peers.get_mut(0).unwrap().execution = RethPeerInfo {
            peering_tcp_addr: "127.0.0.1:30303".parse().unwrap(),
            peer_id: "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".parse().unwrap(),
        };
        let expected_toml_data = toml::to_string(&expected_config).unwrap();
        // for debugging purposes
        println!("{}", expected_toml_data);

        // Deserialize the TOML string into a NodeConfig
        let config = toml::from_str::<NodeConfig>(toml_data)
            .expect("Failed to deserialize NodeConfig from TOML");

        // Assert the entire struct matches
        assert_eq!(config, expected_config);
    }

    #[test]
    fn test_roundtrip_toml_serdes() {
        let cfg = NodeConfig::testnet();
        let enc = toml::to_string_pretty(&cfg).unwrap();
        let dec: NodeConfig = toml::from_str(&enc).unwrap();
        assert_eq!(cfg, dec);
    }
}
