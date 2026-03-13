use crate::hardfork_config::{Aurora, Borealis, FrontierParams, IrysHardforkConfig, Sprite};
use crate::{serde_utils, unix_timestamp_string_serde, UnixTimestamp};
use crate::{
    storage_pricing::{
        phantoms::{
            CostPerChunk, CostPerChunkDurationAdjusted, CostPerGb, DecayRate, Irys, IrysPrice,
            Percentage, Usd,
        },
        Amount,
    },
    IrysAddress, H256,
};
use alloy_core::hex::FromHex as _;
use alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, U256};
use eyre::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::de::value::StringDeserializer;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// # Consensus Configuration
///
/// Defines the core parameters that govern the Irys network consensus rules.
/// These parameters determine how the network operates, including pricing,
/// storage requirements, and data validation mechanisms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsensusConfig {
    /// Unique identifier for the blockchain network
    pub chain_id: u64,

    /// Reth-specific configuration
    pub reth: IrysRethConfig,

    /// Settings for the transaction memory pool
    pub mempool: MempoolConsensusConfig,

    /// Controls how mining difficulty adjusts over time
    pub difficulty_adjustment: DifficultyAdjustmentConfig,

    /// Defines the acceptable range of token price fluctuation between consecutive blocks
    /// This helps prevent price manipulation and ensures price stability
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub token_price_safe_range: Amount<Percentage>,

    /// Genesis-specific config values
    pub genesis: GenesisConfig,

    /// Expected genesis hash (when joining existing networks)
    #[serde(default)]
    pub expected_genesis_hash: Option<H256>,

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
    pub vdf: VdfConsensusConfig,

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

    /// Number of chunks that can be recalled in each partition by a mining step
    pub num_chunks_in_recall_range: u64,

    /// Number of replica partitions in each storage slot
    pub num_partitions_per_slot: u64,

    /// Number of iterations for entropy packing algorithm
    pub entropy_packing_iterations: u32,

    /// Cache management configuration
    pub epoch: EpochConfig,

    /// Configuration for Exponential Moving Average price calculations
    pub ema: EmaConfig,

    /// Hardfork configuration with parameters for each fork.
    pub hardforks: IrysHardforkConfig,

    /// Enable full ingress proof verification against actual chunks during block validation.
    /// When enabled, nodes will reconstruct the data_root from locally available chunks and
    /// verify ingress proofs, enforcing data availability at validation time.
    /// Defaults to `false` to preserve current behavior.
    #[serde(default = "default_disable_full_ingress_proof_validation")]
    pub enable_full_ingress_proof_validation: bool,

    /// Target number of years data should be preserved on the network
    /// Determines long-term storage pricing and incentives
    pub safe_minimum_number_of_years: u64,

    /// Fee required to stake a mining address in Irys tokens
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub stake_value: Amount<Irys>,

    /// Base fee required for pledging a partition in Irys tokens
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub pledge_base_value: Amount<Irys>,

    /// Decay rate for pledge fees - subsequent pledges become cheaper
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub pledge_decay: Amount<Percentage>,

    /// This is the fee that is used for immediate tx inclusion fees:
    /// miner receives: `tx.term_fee * immediate_tx_inclusion_reward_percent`
    ///
    /// This field is also used for immediate ingress proof rewards:
    /// ingress proof producer receives: `tx.term_fee * immediate_tx_inclusion_reward_percent`
    ///
    /// Both of these reward distribution mechanisms are opaque to the user,
    /// the user will only ever see `term_fee` and `perm_fee`.
    #[serde(
        deserialize_with = "serde_utils::percentage_amount",
        serialize_with = "serde_utils::serializes_percentage_amount"
    )]
    pub immediate_tx_inclusion_reward_percent: Amount<Percentage>,

    /// Minimum term fee in USD that must be paid for term storage
    /// If calculated fee is below this threshold, it will be rounded up
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub minimum_term_fee_usd: Amount<(CostPerChunkDurationAdjusted, Usd)>,

    /// Maximum future drift
    #[serde(
        default = "default_max_future_timestamp_drift_millis",
        deserialize_with = "serde_utils::u128_millis_from_u64",
        serialize_with = "serde_utils::u128_millis_to_u64"
    )]
    /// Tolerance for future block timestamps due to clock drift (ms)
    pub max_future_timestamp_drift_millis: u128,
}

/// Default for `max_future_timestamp_drift_millis` when the field is not
/// present in the provided TOML. This keeps legacy configurations working.
fn default_max_future_timestamp_drift_millis() -> u128 {
    15_000
}

/// Default for `enable_full_ingress_proof_validation` when the field is not
/// present in the provided TOML. This preserves current behavior.
fn default_disable_full_ingress_proof_validation() -> bool {
    false
}

/// # Consensus Configuration Source
///
/// Specifies where the node should obtain its consensus rules from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ConsensusOptions {
    /// Load consensus configuration from a file at the specified path
    Path(PathBuf),

    /// Use predefined testnet consensus parameters
    Testnet,

    /// Use predefined testing consensus parameters
    Testing,

    /// Use predefined mainnet consensus parameters
    Mainnet,

    /// Use custom consensus parameters defined elsewhere
    Custom(ConsensusConfig),
}

impl ConsensusOptions {
    pub fn extend_genesis_accounts(
        &mut self,
        accounts: impl IntoIterator<Item = (IrysAddress, GenesisAccount)>,
    ) {
        let config = self.get_mut();
        config
            .reth
            .extend_accounts(accounts.into_iter().map(|a| (a.0.into(), a.1)));
    }

    pub fn get_mut(&mut self) -> &mut ConsensusConfig {
        let Self::Custom(config) = self else {
            panic!("only support mutating custom configs");
        };

        config
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockRewardConfig {
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    /// The total number of tokens emitted by inflation
    pub inflation_cap: Amount<Irys>,
    /// The number of seconds in each emission half life, determines inflation curve
    pub half_life_secs: u64,
}

/// # Reth Configuration
///
/// Minimal configuration needed for reth integration.
/// The full `alloy_genesis::Genesis` is constructed at runtime from these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrysRethConfig {
    /// Gas limit for genesis block (defaults to Ethereum 30M)
    #[serde(default = "default_gas_limit")]
    pub gas_limit: u64,

    /// Initial account allocations (address -> balance/code/storage)
    #[serde(default)]
    pub alloc: BTreeMap<Address, GenesisAccount>,
}

fn default_gas_limit() -> u64 {
    ETHEREUM_BLOCK_GAS_LIMIT_30M
}

impl IrysRethConfig {
    /// Extend the genesis allocations with additional accounts
    pub fn extend_accounts(
        &mut self,
        accounts: impl IntoIterator<Item = (Address, GenesisAccount)>,
    ) {
        self.alloc.extend(accounts);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisConfig {
    /// The timestamp in milliseconds used for the genesis block
    #[serde(
        deserialize_with = "serde_utils::u128_millis_from_u64",
        serialize_with = "serde_utils::u128_millis_to_u64"
    )]
    pub timestamp_millis: u128,

    /// Address that signs the genesis block
    pub miner_address: IrysAddress,

    /// Address that receives the genesis block reward
    pub reward_address: IrysAddress,

    /// The initial last_epoch_hash used by the genesis block
    pub last_epoch_hash: H256,

    /// The initial VDF seed used by the genesis block
    /// Must be explicitly set for deterministic VDF output at genesis.
    pub vdf_seed: H256,

    /// The initial next VDF seed used after the first reset boundary.
    /// If not set in config, defaults to the same value as `vdf_seed`.
    #[serde(default)]
    pub vdf_next_seed: Option<H256>,

    /// The initial price of the Irys token at genesis in USD
    /// Sets the baseline for all future pricing calculations
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub genesis_price: Amount<(IrysPrice, Usd)>,
}

/// # Epoch Configuration
///
/// Controls the timing and parameters for network epochs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EpochConfig {
    /// Scaling factor for the capacity projection curve
    /// Affects how network capacity is calculated and projected
    pub capacity_scalar: u64,

    /// Number of blocks in a single epoch
    pub num_blocks_in_epoch: u64,

    /// Number of epochs before a submit ledger partition expires
    pub submit_ledger_epoch_length: u64,

    /// Optional configuration for capacity provisioning at genesis
    pub num_capacity_partitions: Option<u64>,
}

/// # EMA (Exponential Moving Average) Configuration
///
/// Controls how token prices are smoothed over time to reduce volatility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmaConfig {
    /// Number of blocks between EMA price recalculations
    /// Lower values make prices more responsive, higher values provide more stability
    pub price_adjustment_interval: u64,
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VdfConsensusConfig {
    /// VDF reset frequency in global steps
    /// Formula: blocks_between_resets × vdf_steps_per_block
    /// Example: 50 blocks × 12 steps = 600 global steps
    /// At 12s/block target, resets occur every ~10 minutes
    pub reset_frequency: usize,

    /// Maximum number of threads to use for parallel VDF verification
    /// This is part of the NodeConfig
    // pub parallel_verification_thread_limit: usize,

    /// Number of checkpoints to include in each VDF step
    pub num_checkpoints_in_vdf_step: usize,

    /// Minimum number of steps to store in FIFO VecDeque to allow for network forks
    pub max_allowed_vdf_fork_steps: u64,

    /// Target number of SHA-1 operations per second for VDF calibration
    pub sha_1s_difficulty: u64,
}

/// # Difficulty Adjustment Configuration
///
/// Controls how mining difficulty changes over time to maintain target block times.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// # Mempool Configuration
///
/// Controls how unconfirmed transactions are managed before inclusion in blocks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MempoolConsensusConfig {
    /// Maximum number of data transactions that can be included in a single block
    pub max_data_txs_per_block: u64,

    /// Maximum number of commitment transactions allowed in a single block
    pub max_commitment_txs_per_block: u64,

    /// The number of blocks a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub tx_anchor_expiry_depth: u8,

    /// The number of blocks a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the ingress proof it anchors will drop.
    pub ingress_proof_anchor_expiry_depth: u16,

    /// Fee required for commitment transactions (stake, unstake, pledge, unpledge)
    pub commitment_fee: u64,
}

/// Recursively sort all object keys in a JSON value for deterministic serialization.
fn sort_json_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| (k, sort_json_keys(v)))
                .collect();
            serde_json::Value::Object(
                sorted
                    .into_iter()
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            )
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_json_keys).collect())
        }
        other => other,
    }
}

impl ConsensusConfig {
    /// Produce a deterministic Keccak256 hash of this consensus config.
    ///
    /// Uses canonical JSON serialization with alphabetically sorted keys.
    /// This ensures platform-independent and deterministic hashing across the network.
    pub fn keccak256_hash(&self) -> H256 {
        // Serialize to canonical JSON value (camelCase keys, u64/i64 as strings)
        let json_value = crate::canonical::to_canonical(self)
            .expect("ConsensusConfig should serialize to canonical JSON");

        // Sort all keys recursively for deterministic ordering
        let sorted_value = sort_json_keys(json_value);

        // Serialize to compact JSON string (no extra whitespace)
        let json_string = serde_json::to_string(&sorted_value)
            .expect("Sorted JSON value should serialize to string");

        // Hash the canonical JSON string
        H256(alloy_primitives::keccak256(json_string.as_bytes()).0)
    }

    // This is hardcoded here to be used just by C packing related stuff as it is also hardcoded right now in C sources
    // TODO: get rid of this hardcoded variable? Otherwise altering the `chunk_size` in the configs may have
    // discrepancies when using GPU mining
    pub const CHUNK_SIZE: u64 = 256 * 1024;

    // 20TB, with ~10% overhead, aligned to the nearest recall range (400 chunks)
    pub const CHUNKS_PER_PARTITION_20TB: u64 = 75_534_400;

    /// Calculate the number of epochs in one year based on network parameters
    pub fn epochs_per_year(&self) -> u64 {
        const SECONDS_PER_YEAR: u64 = 365 * 24 * 60 * 60;
        let seconds_per_epoch =
            self.difficulty_adjustment.block_time * self.epoch.num_blocks_in_epoch;
        SECONDS_PER_YEAR / seconds_per_epoch
    }

    /// Convert years to epochs based on network parameters
    pub fn years_to_epochs(&self, years: u64) -> u64 {
        years * self.epochs_per_year()
    }

    /// Compute cost per chunk per epoch from annual cost per GB
    pub fn cost_per_chunk_per_epoch(&self) -> Result<Amount<(CostPerChunk, Usd)>> {
        const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;
        let chunks_per_gb = BYTES_PER_GB / self.chunk_size;
        let epochs_per_year = self.epochs_per_year();

        // Convert annual_cost_per_gb to cost_per_chunk_per_epoch
        // annual_cost_per_gb / chunks_per_gb / epochs_per_year
        let annual_decimal = self.annual_cost_per_gb.token_to_decimal()?;
        let cost_per_chunk_per_year = annual_decimal / Decimal::from(chunks_per_gb);
        let cost_per_chunk_per_epoch = cost_per_chunk_per_year / Decimal::from(epochs_per_year);

        Amount::token(cost_per_chunk_per_epoch)
    }

    pub fn mainnet() -> Self {
        const IRYS_MAINNET_CHAIN_ID: u64 = 3282;
        const IRYS_BLOCK_TIME: u64 = 12;

        // block reward params
        const HALF_LIFE_YEARS: u128 = 4;
        const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;
        const INFLATION_CAP: u128 = 1_300_000_000;

        Self {
            chain_id: IRYS_MAINNET_CHAIN_ID,
            // The annual cost in USD for storing 1GB of data on the Irys network Used as the foundation for calculating storage fees
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(), // 0.01$
            // Annual rate at which storage costs are expected to decrease Accounts for technological improvements making storage cheaper over time
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(), // 1%
            // Target number of years data should be preserved on the network Determines long-term storage pricing and incentives
            safe_minimum_number_of_years: 200,
            // Size of each data chunk in bytes
            chunk_size: 256 * 1024, // 256KiB
            // Defines the number of confirmations before a block is migrated to the index
            block_migration_depth: 6,
            // Number of blocks to retain in the block tree from chain head
            block_tree_depth: 100,
            // Configures how mining difficulty changes over time to maintain target block times
            difficulty_adjustment: DifficultyAdjustmentConfig {
                // Target time between blocks in seconds
                block_time: IRYS_BLOCK_TIME,
                // Number of blocks between difficulty adjustments
                difficulty_adjustment_interval: 2000,
                // Maximum factor by which difficulty can increase in a single adjustment
                max_difficulty_adjustment_factor: dec!(4),
                // Minimum factor by which difficulty can decrease in a single adjustment
                min_difficulty_adjustment_factor: dec!(0.25),
            },
            // Configures inflation parameters
            block_reward_config: BlockRewardConfig {
                // The total number of tokens emitted by inflation
                inflation_cap: Amount::token(rust_decimal::Decimal::from(INFLATION_CAP)).unwrap(),
                // The number of seconds in each emission half life, determines inflation curve
                half_life_secs: (HALF_LIFE_YEARS * SECS_PER_YEAR).try_into().unwrap(),
            },
            // Reth config
            reth: IrysRethConfig {
                gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
                alloc: {
                    let mut map = BTreeMap::new();
                    map.insert(
                        Address::from_slice(
                            hex::decode("3f0b21c8641c8cB28A9381fEa4F619B8d11dD35c")
                                .unwrap()
                                .as_slice(),
                        ),
                        GenesisAccount {
                            // 10 billion IRYS
                            balance: U256::from(10 * 1_000_000_000_u128)
                                * U256::from(1_000_000_000_000_000_000_u128),
                            ..Default::default()
                        },
                    );
                    map
                },
            },
            genesis: GenesisConfig {
                // The timestamp in milliseconds used for the genesis block
                timestamp_millis: 1763749823171,
                // Address that signs the genesis block
                miner_address: IrysAddress::from_hex("faf11d0e472d0b2dc4dab8d4817d0854e3f9e03e")
                    .unwrap(), // todo()
                // Address that receives the genesis block reward
                reward_address: IrysAddress::from_hex("faf11d0e472d0b2dc4dab8d4817d0854e3f9e03e")
                    .unwrap(), // todo()
                // The initial last_epoch_hash used by the genesis block
                last_epoch_hash: H256::from_base58("4eiVupeZoaBgxyDMacRHnvbbzodLRTEcGUG3yjXpAE4i"),
                // The initial VDF seed used by the genesis block Must be explicitly set for deterministic VDF output at genesis.
                vdf_seed: H256::from_base58("3peqVpX6VF8GjEsSeFk6XPN19bSn7fQbjgT6PJMkpzDo"),
                // The initial next VDF seed used after the first reset boundary. If not set in config, defaults to the same value as vdf_seed
                vdf_next_seed: None,
                // The initial price of the Irys token at genesis in USD Sets the baseline for all future pricing calculations
                genesis_price: Amount::token(dec!(0.15)).expect("valid token amount"),
            },
            mempool: MempoolConsensusConfig {
                // Maximum number of data transactions that can be included in a single block
                max_data_txs_per_block: 100,
                // Maximum number of commitment transactions allowed in a single block
                max_commitment_txs_per_block: 100,
                // The number of blocks a given anchor (tx or block hash) is valid for. The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
                tx_anchor_expiry_depth: 20,
                // The number of blocks a given anchor (tx or block hash) is valid for. The anchor must be included within the last X blocks otherwise the ingress proof it anchors will drop.
                ingress_proof_anchor_expiry_depth: 200,
                // Fee required for commitment transactions (stake, unstake, pledge, unpledge)
                commitment_fee: 62500000000000000,
            },
            epoch: EpochConfig {
                // Scaling factor for the capacity projection curve Affects how network capacity is calculated and projected
                capacity_scalar: 100,
                // Number of blocks in a single epoch
                num_blocks_in_epoch: 60_u64.div_ceil(IRYS_BLOCK_TIME) * 60 * 24, // Number of blocks in a day (7,200)
                // Number of epochs before a submit ledger partition expires
                submit_ledger_epoch_length: 5,
                // Optional configuration for capacity provisioning at genesis
                num_capacity_partitions: None,
            },
            // Number of blocks between EMA price recalculations Lower values make prices more responsive, higher values provide more stability
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            // Defines the acceptable range of token price fluctuation between consecutive blocks This helps prevent price manipulation and ensures price stability
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            // Settings for the time-delay proof mechanism used in consensus.
            vdf: VdfConsensusConfig {
                // Reset VDF every ~50 blocks (50 blocks × 12 steps/block = 600 global steps)
                // With 12s target block time, this resets approximately every 10 minutes
                reset_frequency: 50 * 12,
                // Number of checkpoints to include in each VDF step
                num_checkpoints_in_vdf_step: 25,
                // Minimum number of steps to store in FIFO VecDeque to allow for network forks
                // num_chunks_in_partition / num_chunks_in_recall_range + some safety buffer for forks
                max_allowed_vdf_fork_steps: 190_000,
                // Target number of SHA-256 operations per second for the VDF
                sha_1s_difficulty: 13_000_000,
            },
            // Number of chunks that make up a single partition
            num_chunks_in_partition: 75_534_400, //  ~20 TB,
            // Number of chunks that can be recalled in each partition by a mining step
            num_chunks_in_recall_range: 400,
            // Number of replica partitions in each storage slot
            num_partitions_per_slot: 10,
            // Number of iterations for Matrix (entropy) packing algorithm
            entropy_packing_iterations: 1_000_000,
            // Toggles full ingress proof validation on or off
            enable_full_ingress_proof_validation: false,
            // Fee required to stake a mining address in Irys tokens
            stake_value: Amount::token(dec!(400_000)).expect("valid token amount"),
            // Base fee required for pledging a partition in Irys tokens
            pledge_base_value: Amount::token(dec!(14_000)).expect("valid token amount"),
            // Decay rate for pledge fees - subsequent pledges become cheaper
            pledge_decay: Amount::percentage(dec!(0.9)).expect("valid percentage"),
            // Percentage of the storage fee used to reward the miner for inclusion
            immediate_tx_inclusion_reward_percent: Amount::percentage(dec!(0.05))
                .expect("valid percentage"),
            // Minimum term fee in USD that must be paid for term storage If calculated fee is below this threshold, it will be rounded up
            minimum_term_fee_usd: Amount::token(dec!(0.01)).expect("valid token amount"), // $0.01 USD minimum,
            // Tolerance for future block timestamps due to clock drift (ms)
            max_future_timestamp_drift_millis: 15_000,
            // Expected genesis block hash (when joining existing networks)
            expected_genesis_hash: Some(H256::from_base58(
                "2Pgf5vJvvFifTnyJy9gTg31Yba2HFh2hC6imqsmJqdF7",
            )),
            // Hardfork configuration - mainnet values
            hardforks: IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 10,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                sprite: None,
                aurora: None,
                borealis: None,
            },
        }
    }

    pub fn testing() -> Self {
        const DEFAULT_BLOCK_TIME: u64 = 1;
        const CHUNK_SIZE: u64 = 32;
        const SHA1_DIFFICULTY: u64 = 70_000;
        const TEST_NUM_CHUNKS_IN_PARTITION: u64 = 10;
        const TEST_NUM_CHUNKS_IN_RECALL_RANGE: u64 = 2;
        const HALF_LIFE_YEARS: u128 = 4;
        const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;
        const INFLATION_CAP: u128 = 100_000_000;

        Self {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(),
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),
            safe_minimum_number_of_years: 200,
            genesis: GenesisConfig {
                timestamp_millis: 0,
                miner_address: IrysAddress::ZERO,
                reward_address: IrysAddress::ZERO,
                last_epoch_hash: H256::zero(), // Critical: must be zero for deterministic tests
                vdf_seed: H256::zero(),
                vdf_next_seed: None,
                genesis_price: Amount::token(dec!(1)).expect("valid token amount"),
            },
            expected_genesis_hash: None,
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            chunk_size: CHUNK_SIZE,
            num_chunks_in_partition: TEST_NUM_CHUNKS_IN_PARTITION,
            num_chunks_in_recall_range: TEST_NUM_CHUNKS_IN_RECALL_RANGE,
            num_partitions_per_slot: 1,
            block_migration_depth: 6,
            block_tree_depth: 50,
            entropy_packing_iterations: 1000,
            stake_value: Amount::token(dec!(20000)).expect("valid token amount"),
            pledge_base_value: Amount::token(dec!(950)).expect("valid token amount"),
            pledge_decay: Amount::percentage(dec!(0.9)).expect("valid percentage"),
            mempool: MempoolConsensusConfig {
                max_data_txs_per_block: 100,
                max_commitment_txs_per_block: 100,
                tx_anchor_expiry_depth: 20,
                ingress_proof_anchor_expiry_depth: 200,
                commitment_fee: 100,
            },
            vdf: VdfConsensusConfig {
                reset_frequency: 50 * 12,
                num_checkpoints_in_vdf_step: 25,
                max_allowed_vdf_fork_steps: 60_000,
                sha_1s_difficulty: SHA1_DIFFICULTY,
            },
            epoch: EpochConfig {
                capacity_scalar: 100,
                num_blocks_in_epoch: 100,
                submit_ledger_epoch_length: 5,
                num_capacity_partitions: None,
            },
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
            reth: IrysRethConfig {
                gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
                alloc: {
                    let mut map = BTreeMap::new();
                    map.insert(
                        Address::from_hex("64f1a2829e0e698c18e7792d6e74f67d89aa0a32")
                            .expect("valid hex address"),
                        GenesisAccount {
                            balance: U256::from(99999000000000000000000_u128),
                            ..Default::default()
                        },
                    );
                    map.insert(
                        Address::from_hex("A93225CBf141438629f1bd906A31a1c5401CE924")
                            .expect("valid hex address"),
                        GenesisAccount {
                            balance: U256::from(99999000000000000000000_u128),
                            ..Default::default()
                        },
                    );
                    map
                },
            },
            block_reward_config: BlockRewardConfig {
                inflation_cap: Amount::token(rust_decimal::Decimal::from(INFLATION_CAP)).unwrap(),
                half_life_secs: (HALF_LIFE_YEARS * SECS_PER_YEAR).try_into().unwrap(),
            },
            immediate_tx_inclusion_reward_percent: Amount::percentage(dec!(0.05))
                .expect("valid percentage"),
            minimum_term_fee_usd: Amount::token(dec!(0.01)).expect("valid token amount"),
            enable_full_ingress_proof_validation: false,
            max_future_timestamp_drift_millis: 15_000,
            hardforks: IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1, // Only 1 proof needed in tests
                    number_of_ingress_proofs_from_assignees: 0,
                },
                sprite: Some(Sprite {
                    // From feat/pd - enable PD from genesis for tests
                    activation_timestamp: UnixTimestamp::from_secs(0),
                    cost_per_mb: Amount::token(dec!(0.01)).expect("valid token amount"),
                    base_fee_floor: Amount::token(dec!(0.01)).expect("valid token amount"),
                    max_pd_chunks_per_block: 7_500,
                    min_pd_transaction_cost: Amount::token(dec!(0.01)).expect("valid token amount"),
                }),
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(1768476600),
                    minimum_commitment_tx_version: 2,
                }),
                next_name_tbd: None,
                // Borealis hardfork - enabled from genesis for testing
                borealis: Some(Borealis {
                    activation_timestamp: UnixTimestamp::from_secs(0),
                }),
            },
        }
    }

    pub fn testnet() -> Self {
        const BLOCK_TIME: u64 = 10;
        const INFLATION_CAP: u128 = 100_000_000;
        const HALF_LIFE_YEARS: u128 = 4;
        const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;

        Self {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(), // 0.01$
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),    // 1%
            safe_minimum_number_of_years: 200,

            expected_genesis_hash: Some(H256::from_base58(
                "CVqXN2QqETJYke83dKCMjBWEnxstrFGk6ySUqNho7QRr",
            )),
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            chunk_size: Self::CHUNK_SIZE,
            num_chunks_in_partition: Self::CHUNKS_PER_PARTITION_20TB,
            num_chunks_in_recall_range: 400,
            num_partitions_per_slot: 10,
            block_migration_depth: 6,
            block_tree_depth: 50,
            entropy_packing_iterations: 1000,
            stake_value: Amount::token(dec!(20000)).expect("valid token amount"),
            pledge_base_value: Amount::token(dec!(950)).expect("valid token amount"),
            pledge_decay: Amount::percentage(dec!(0.9)).expect("valid percentage"),
            immediate_tx_inclusion_reward_percent: Amount::percentage(dec!(0.05))
                .expect("valid percentage"),
            minimum_term_fee_usd: Amount::token(dec!(0.01)).expect("valid token amount"), // $0.01 USD minimum
            enable_full_ingress_proof_validation: false,
            max_future_timestamp_drift_millis: 15_000,

            genesis: GenesisConfig {
                timestamp_millis: 1764677430138,
                miner_address: IrysAddress::from_hex("0x577b412bc03804496a1f787280c66dcd82873375")
                    .unwrap(),
                reward_address: IrysAddress::from_hex("0x577b412bc03804496a1f787280c66dcd82873375")
                    .unwrap(),
                last_epoch_hash: H256::from_base58("6mZBRJGrxbYZsLLQqwZEFEAsdvNvx4Hd7RVVBAD69f7Y"),
                vdf_seed: H256::zero(),
                vdf_next_seed: None,
                genesis_price: Amount::token(dec!(1)).expect("valid token amount"),
            },

            mempool: MempoolConsensusConfig {
                max_data_txs_per_block: 100,
                max_commitment_txs_per_block: 100,
                tx_anchor_expiry_depth: 50,
                ingress_proof_anchor_expiry_depth: 200,
                commitment_fee: 100,
            },
            vdf: VdfConsensusConfig {
                reset_frequency: 1200,
                num_checkpoints_in_vdf_step: 25,
                max_allowed_vdf_fork_steps: 190_735,
                sha_1s_difficulty: 10_000_000,
            },

            epoch: EpochConfig {
                capacity_scalar: 100,
                num_blocks_in_epoch: 360,
                submit_ledger_epoch_length: 5,
                num_capacity_partitions: None,
            },

            difficulty_adjustment: DifficultyAdjustmentConfig {
                block_time: BLOCK_TIME,
                difficulty_adjustment_interval: 2000,
                max_difficulty_adjustment_factor: dec!(4),
                min_difficulty_adjustment_factor: dec!(0.25),
            },
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            reth: IrysRethConfig {
                gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
                alloc: BTreeMap::new(),
            },
            block_reward_config: BlockRewardConfig {
                inflation_cap: Amount::token(rust_decimal::Decimal::from(INFLATION_CAP)).unwrap(),
                half_life_secs: (HALF_LIFE_YEARS * SECS_PER_YEAR).try_into().unwrap(),
            },

            // Hardfork configuration
            hardforks: IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 3,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                aurora: Some(Aurora {
                    activation_timestamp: unix_timestamp_string_serde::deserialize(
                        StringDeserializer::<serde::de::value::Error>::new(
                            "2026-01-29T16:30:00+00:00".to_owned(),
                        ),
                    )
                    .unwrap(),
                    minimum_commitment_tx_version: 2,
                }),
                next_name_tbd: None,
                sprite: None,
                // Borealis hardfork - disabled for testnet (controlled activation)
                borealis: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consensus_hash_deterministic() {
        let config = ConsensusConfig::testing();
        let hash1 = config.keccak256_hash();
        let hash2 = config.keccak256_hash();
        assert_eq!(hash1, hash2, "same config should hash to the same value");
        assert_ne!(hash1, H256::zero(), "hash should not be zero");
    }

    #[test]
    fn test_consensus_hash_differs_on_change() {
        let config_a = ConsensusConfig::testing();
        let mut config_b = ConsensusConfig::testing();
        config_b.chunk_size += 1;
        assert_ne!(
            config_a.keccak256_hash(),
            config_b.keccak256_hash(),
            "configs differing by one field should produce different hashes"
        );
    }

    #[test]
    fn test_consensus_hash_independent_instances() {
        let config_a = ConsensusConfig::testing();
        let config_b = ConsensusConfig::testing();
        assert_eq!(
            config_a.keccak256_hash(),
            config_b.keccak256_hash(),
            "independently constructed configs should produce identical hashes"
        );
    }

    #[test]
    fn test_genesis_and_peer_consensus_hash_match() {
        let mut genesis_config = ConsensusConfig::testing();
        assert!(genesis_config.expected_genesis_hash.is_none());

        let fake_hash = H256::from_base58("5VoHFxVrC4WM7VHDwUJrFWZ2yVJXkY3JHEsR2U9bQxXH");

        let mut peer_config = ConsensusConfig::testing();
        peer_config.expected_genesis_hash = Some(fake_hash);

        // Simulate what Genesis node does at runtime
        genesis_config.expected_genesis_hash = Some(fake_hash);

        assert_eq!(
            genesis_config.keccak256_hash(),
            peer_config.keccak256_hash(),
            "Genesis and Peer nodes with same expected_genesis_hash must have matching consensus hashes"
        );
    }

    #[test]
    fn test_consensus_hash_regression() {
        // This test verifies that the hash of the testing config remains stable.
        // If this test fails, it indicates a breaking change in either:
        // - The ConsensusConfig structure or field order
        // - The canonical JSON serialization implementation
        // - The serde serialization of dependency types
        let config = ConsensusConfig::testing();
        let expected_hash = H256::from_base58("JAvW3YLPGQhoDcR4xdGyh6fssC6usQh7gGkh5fC2JPTM");
        assert_eq!(
            config.keccak256_hash(),
            expected_hash,
            "Hash changed—this may indicate a breaking change in the consensus config or its dependencies"
        );
    }
}
