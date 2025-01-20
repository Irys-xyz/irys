use irys_macros::load_toml;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{DifficultyAdjustmentConfig, U256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Block time in seconds
    pub block_time: u64,
    pub max_data_txs_per_block: u64,
    pub difficulty_adjustment_interval: u64,
    pub max_difficulty_adjustment_factor: Decimal,
    pub min_difficulty_adjustment_factor: Decimal,
    pub chunk_size: u64,
    pub num_chunks_in_partition: u64,
    pub num_chunks_in_recall_range: u64,
    pub vdf_reset_frequency: usize,
    pub vdf_parallel_verification_thread_limit: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_sha_1s: u64,
    pub entropy_packing_iterations: u32,
    pub irys_chain_id: u64,
    /// Scaling factor for the capacity projection curve
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_partitions_per_slot: u64,
    pub num_writes_before_sync: u64,
    /// If `true`, the ledger will be persisted on disk when the node restarts. Otherwise the
    /// entire state of the node will reset to genesis upon restart.
    pub persist_data_on_restart: bool,
    // Longest chain consensus
    /// Number of block confirmations required before considering data final.
    ///
    /// In Nakamoto consensus, finality is probabilistic based on chain depth:
    /// - 6 confirmations protects against attackers with <25% hashpower
    /// - 20 confirmations protects against attackers with <40% hashpower
    /// - No number of confirmations is secure against attackers with >50% hashpower
    pub num_confirmations_for_finality: u32,
    pub mining_key: &'static str,
    pub num_capacity_partitions: Option<u64>
}

pub const DEFAULT_BLOCK_TIME: u64 = 1;

pub const CONFIG: Config = load_toml!(
    "CONFIG_TOML_PATH",
    Config {
        block_time: DEFAULT_BLOCK_TIME,
        max_data_txs_per_block: 100,
        difficulty_adjustment_interval: (24u64 * 60 * 60 * 1000).div_ceil(DEFAULT_BLOCK_TIME) * 14, // 2 weeks worth of blocks
        max_difficulty_adjustment_factor: rust_decimal_macros::dec!(4), // A difficulty adjustment can be 4x larger or 1/4th the current difficulty
        min_difficulty_adjustment_factor: rust_decimal_macros::dec!(0.25), // A 10% change must be required before a difficulty adjustment will occur
        chunk_size: 256 * 1024,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        vdf_reset_frequency: 10 * 120, // Reset the nonce limiter (vdf) once every 1200 steps/seconds or every ~20 min
        vdf_parallel_verification_thread_limit: 4,
        num_checkpoints_in_vdf_step: 25, // 25 checkpoints 40 ms each = 1000 ms
        vdf_sha_1s: 530_000,
        entropy_packing_iterations: 22_500_000,
        irys_chain_id: 69727973, // "irys" in ascii
        capacity_scalar: 100,
        num_blocks_in_epoch: 100,
        submit_ledger_epoch_length: 5,
        num_partitions_per_slot: 1,
        num_writes_before_sync: 5,
        persist_data_on_restart: false,
        num_confirmations_for_finality: 6,
        mining_key: "",
        num_capacity_partitions: None
    }
);

pub const PARTITION_SIZE: u64 = CONFIG.chunk_size * CONFIG.num_chunks_in_partition;
pub const NUM_RECALL_RANGES_IN_PARTITION: u64 =
    CONFIG.num_chunks_in_partition / CONFIG.num_chunks_in_recall_range;

impl From<Config> for DifficultyAdjustmentConfig {
    fn from(config: Config) -> Self {
        DifficultyAdjustmentConfig {
            target_block_time: config.block_time,
            adjustment_interval: config.difficulty_adjustment_interval,
            max_adjustment_factor: config.max_difficulty_adjustment_factor,
            min_adjustment_factor: config.min_difficulty_adjustment_factor,
            min_difficulty: U256::one(), // TODO: make this customizable if desirable
            max_difficulty: U256::MAX,
        }
    }
}
