use crate::serde_utils;
use crate::{
    storage_pricing::{
        phantoms::{
            CostPerChunk, CostPerChunkDurationAdjusted, CostPerGb, DecayRate, Irys, IrysPrice,
            Percentage, Usd,
        },
        Amount,
    },
    H256,
};
use alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::Address;
use eyre::Result;
use reth_chainspec::Chain;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
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

    /// Reth chain spec for the reth genesis
    pub reth: RethChainSpec,

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

    /// The initial price of the Irys token at genesis in USD
    /// Sets the baseline for all future pricing calculations
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub genesis_price: Amount<(IrysPrice, Usd)>,

    /// Genesis-specific config values
    pub genesis: GenesisConfig,

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
    pub number_of_ingress_proofs_total: u64,

    /// Minimum number of proofs from miners assigned to store the associated data
    /// required for data to be promoted
    pub number_of_ingress_proofs_from_assignees: u64,

    /// Target number of years data should be preserved on the network
    /// Determines long-term storage pricing and incentives
    pub safe_minimum_number_of_years: u64,

    /// Fee required for staking operations in Irys tokens
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub stake_value: Amount<Irys>,

    /// Base fee required for pledging operations in Irys tokens
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
    pub max_future_timestamp_drift_millis: u128,
}

/// Default for `max_future_timestamp_drift_millis` when the field is not
/// present in the provided TOML. This keeps legacy configurations working.
fn default_max_future_timestamp_drift_millis() -> u128 {
    15_000
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockRewardConfig {
    #[serde(
        deserialize_with = "serde_utils::token_amount",
        serialize_with = "serde_utils::serializes_token_amount"
    )]
    pub inflation_cap: Amount<Irys>,
    pub half_life_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RethChainSpec {
    /// The type of chain.
    pub chain: Chain,
    /// Genesis block.
    pub genesis: Genesis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisConfig {
    /// The timestamp in milliseconds used for the genesis block
    #[serde(
        deserialize_with = "serde_utils::u128_millis_from_u64",
        serialize_with = "serde_utils::u128_millis_to_u64"
    )]
    pub timestamp_millis: u128,

    /// Address that signs the genesis block
    pub miner_address: Address,

    /// Address that receives the genesis block reward
    pub reward_address: Address,

    /// The initial last_epoch_hash used by the genesis block
    pub last_epoch_hash: H256,

    /// The initial VDF seed used by the genesis block
    /// Must be explicitly set for deterministic VDF output at genesis.
    pub vdf_seed: H256,

    /// The initial next VDF seed used after the first reset boundary.
    /// If not set in config, defaults to the same value as `vdf_seed`.
    #[serde(default)]
    pub vdf_next_seed: Option<H256>,
}

/// # Epoch Configuration
///
/// Controls the timing and parameters for network epochs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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

/// # EMA (Exponential Moving Average) Configuration
///
/// Controls how token prices are smoothed over time to reduce volatility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmaConfig {
    /// Number of blocks between EMA price recalculations
    /// Lower values make prices more responsive, higher values provide more stability
    pub price_adjustment_interval: u64,
}

/// # VDF (Verifiable Delay Function) Configuration
///
/// Settings for the time-delay proof mechanism used in consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MempoolConsensusConfig {
    /// Maximum number of data transactions that can be included in a single block
    pub max_data_txs_per_block: u64,

    /// Maximum number of commitment transactions allowed in a single block
    pub max_commitment_txs_per_block: u64,

    /// The number of blocks a given anchor (tx or block hash) is valid for.
    /// The anchor must be included within the last X blocks otherwise the transaction it anchors will drop.
    pub anchor_expiry_depth: u8,

    /// Fee required for commitment transactions (stake, unstake, pledge, unpledge)
    pub commitment_fee: u64,
}

impl ConsensusConfig {
    // This is hardcoded here to be used just by C packing related stuff as it is also hardcoded right now in C sources
    // TODO: get rid of this hardcoded variable? Otherwise altering the `chunk_size` in the configs may have
    // discrepancies when using GPU mining
    pub const CHUNK_SIZE: u64 = 256 * 1024;

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

    // this is a config used for testing
    pub fn testing() -> Self {
        const DEFAULT_BLOCK_TIME: u64 = 1;
        const IRYS_TESTNET_CHAIN_ID: u64 = 1270;
        const CHUNK_SIZE: u64 = 32;
        // block reward params
        const HALF_LIFE_YEARS: u128 = 4;
        const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60;
        const INFLATION_CAP: u128 = 100_000_000;

        Self {
            chain_id: 1270,
            annual_cost_per_gb: Amount::token(dec!(0.01)).unwrap(), // 0.01$
            decay_rate: Amount::percentage(dec!(0.01)).unwrap(),    // 1%
            safe_minimum_number_of_years: 200,
            number_of_ingress_proofs_total: 1,
            number_of_ingress_proofs_from_assignees: 0,
            genesis_price: Amount::token(dec!(1)).expect("valid token amount"),
            genesis: GenesisConfig {
                timestamp_millis: 0,
                miner_address: Address::ZERO,
                reward_address: Address::ZERO,
                last_epoch_hash: H256::zero(),
                vdf_seed: H256::zero(),
                vdf_next_seed: None,
            },
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            mempool: MempoolConsensusConfig {
                max_data_txs_per_block: 100,
                max_commitment_txs_per_block: 100,
                anchor_expiry_depth: 20,
                commitment_fee: 100,
            },
            vdf: VdfConsensusConfig {
                // Reset VDF every ~50 blocks (50 blocks × 12 steps/block = 600 global steps)
                // With 12s target block time, this resets approximately every 10 minutes
                reset_frequency: 50 * 12,
                num_checkpoints_in_vdf_step: 25,
                max_allowed_vdf_fork_steps: 60_000,
                sha_1s_difficulty: 70_000,
            },
            chunk_size: CHUNK_SIZE,
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
                                balance: alloy_primitives::U256::from(99999000000000000000000_u128),
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
                                balance: alloy_primitives::U256::from(99999000000000000000000_u128),
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
            stake_value: Amount::token(dec!(20000)).expect("valid token amount"),
            pledge_base_value: Amount::token(dec!(950)).expect("valid token amount"),
            pledge_decay: Amount::percentage(dec!(0.9)).expect("valid percentage"),
            immediate_tx_inclusion_reward_percent: Amount::percentage(dec!(0.05))
                .expect("valid percentage"),
            minimum_term_fee_usd: Amount::token(dec!(0.01)).expect("valid token amount"), // $0.01 USD minimum
            max_future_timestamp_drift_millis: 15_000,
        }
    }

    pub fn testnet() -> Self {
        const DEFAULT_BLOCK_TIME: u64 = 12;
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
            number_of_ingress_proofs_total: 1,
            number_of_ingress_proofs_from_assignees: 0,
            genesis_price: Amount::token(dec!(1)).expect("valid token amount"),
            genesis: GenesisConfig {
                timestamp_millis: 0,
                miner_address: Address::ZERO,
                reward_address: Address::ZERO,
                last_epoch_hash: H256::zero(),
                vdf_seed: H256::zero(),
                vdf_next_seed: None,
            },
            token_price_safe_range: Amount::percentage(dec!(1)).expect("valid percentage"),
            chunk_size: Self::CHUNK_SIZE,
            num_chunks_in_partition: 51_872_000,
            num_chunks_in_recall_range: 800,
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
                anchor_expiry_depth: 20,
                commitment_fee: 100,
            },
            vdf: VdfConsensusConfig {
                // Reset VDF every ~50 blocks (50 blocks × 12 steps/block = 600 global steps)
                // With 12s target block time, this resets approximately every 10 minutes
                reset_frequency: 50 * 12,
                num_checkpoints_in_vdf_step: 25,
                max_allowed_vdf_fork_steps: 60_000,
                sha_1s_difficulty: 1_800_000,
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
                                balance: alloy_primitives::U256::from(99999000000000000000000_u128),
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
                                balance: alloy_primitives::U256::from(99999000000000000000000_u128),
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
            immediate_tx_inclusion_reward_percent: Amount::percentage(dec!(0.05))
                .expect("valid percentage"),
            minimum_term_fee_usd: Amount::token(dec!(0.01)).expect("valid token amount"), // $0.01 USD minimum
            max_future_timestamp_drift_millis: 15_000,
        }
    }
}
