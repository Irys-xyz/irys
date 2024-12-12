use k256::elliptic_curve::consts::U25;

use crate::{
    StorageConfig, BLOCK_TIME, DIFFICULTY_ADJUSTMENT_FACTOR, DIFFICULTY_ADJUSTMENT_INTERVAL, U256,
};

#[derive(Debug, Clone)]
pub struct DifficultyAdjustmentConfig {
    /// Desired block time in milliseconds.
    pub target_block_time: u64,
    /// Number of blocks between difficulty adjustments.
    pub adjustment_interval: u64,
    /// Factor for smoothing difficulty adjustments.
    pub adjustment_factor: f64,
    /// Minimum difficulty allowed.
    pub min_difficulty: U256,
    /// Maximum difficulty allowed.
    pub max_difficulty: U256,
}

impl Default for DifficultyAdjustmentConfig {
    fn default() -> Self {
        Self {
            target_block_time: BLOCK_TIME,                       // Default to 30s
            adjustment_interval: DIFFICULTY_ADJUSTMENT_INTERVAL, // 2 weeks worth of blocks
            adjustment_factor: DIFFICULTY_ADJUSTMENT_FACTOR,     // Cap adjustments to 4x or 1/4x
            min_difficulty: U256::from(1),
            max_difficulty: U256::MAX,
        }
    }
}

// let block_hashrate = max_difficulty / (max_difficulty - difficulty);
pub fn get_initial_difficulty(
    difficulty_config: &DifficultyAdjustmentConfig,
    storage_config: &StorageConfig,
    storage_module_count: u64,
) -> U256 {
    let hashes_per_partition_per_second = storage_config.num_chunks_in_recall_range;
    let node_hashes_per_second = U256::from(hashes_per_partition_per_second * storage_module_count);
    let block_hashrate =
        node_hashes_per_second * (difficulty_config.target_block_time.div_ceil(1000));

    // Compute the expected hash rate
    if block_hashrate == U256::zero() {
        panic!("block_hashrate cannot be zero");
    }

    let max_difficulty = difficulty_config.max_difficulty;
    let initial_difficulty = max_difficulty - (max_difficulty / block_hashrate);
    initial_difficulty
}
