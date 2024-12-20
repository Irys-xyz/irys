use std::time::Duration;

use crate::{
    StorageConfig, BLOCK_TIME, DIFFICULTY_ADJUSTMENT_INTERVAL, MAX_DIFFICULTY_ADJUSTMENT_FACTOR,
    MIN_DIFFICULTY_ADJUSTMENT_FACTOR, U256,
};

#[derive(Debug, Clone)]
pub struct DifficultyAdjustmentConfig {
    /// Desired block time in seconds.
    pub target_block_time: u64,
    /// Number of blocks between difficulty adjustments.
    pub adjustment_interval: u64,
    /// Factor for smoothing difficulty adjustments.
    pub max_adjustment_factor: u64,
    /// Factor for smoothing difficulty adjustments.
    pub min_adjustment_factor: f64,
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
            max_adjustment_factor: MAX_DIFFICULTY_ADJUSTMENT_FACTOR, // Cap adjustments to 4x or 1/4x
            min_adjustment_factor: MIN_DIFFICULTY_ADJUSTMENT_FACTOR, // Minimum adjustment threshold is 25%
            min_difficulty: U256::from(1),
            max_difficulty: U256::MAX,
        }
    }
}

pub fn calculate_initial_difficulty(
    difficulty_config: &DifficultyAdjustmentConfig,
    storage_config: &StorageConfig,
    storage_module_count: u64,
) -> Result<U256, &'static str> {
    let hashes_per_sec = storage_config.num_chunks_in_recall_range * storage_module_count;
    let block_time = difficulty_config.target_block_time;

    if hashes_per_sec == 0 || block_time == 0 {
        return Err("Input values cannot be zero");
    }

    let max_diff = U256::MAX;
    let block_hashrate = U256::from(hashes_per_sec) * U256::from(block_time);

    let initial_difficulty = max_diff - (max_diff / block_hashrate);
    let target = max_diff - initial_difficulty;

    // Rearranged operations to avoid overflow
    let scale = U256::from(1_000_000);
    let probability_per_hash = target / (max_diff / scale); // Divide max first
    let expected_hashes = scale / probability_per_hash;

    println!("Block hashrate: {}", block_hashrate);
    println!("Initial difficulty: {}", initial_difficulty);
    println!("Target: {}", target);
    println!("Probability per hash (×10^-6): {}", probability_per_hash);
    println!("Expected hashes to find block: {}", expected_hashes);

    Ok(initial_difficulty)
}

pub fn adjust_difficulty(
    current_difficulty: U256,
    actual_time_ms: u128,
    target_time_ms: u128,
) -> U256 {
    let max_u256 = U256::MAX;
    let scale = U256::from(1000);

    // For time ratio, if actual > target, divide first
    // If actual < target, multiply first
    let adjustment_ratio = if actual_time_ms >= target_time_ms {
        let ratio = U256::from(actual_time_ms / target_time_ms);
        ratio * scale
    } else {
        // actual is smaller than target, safe to multiply first
        (U256::from(actual_time_ms) * scale) / U256::from(target_time_ms)
    };

    let target_current = max_u256 - current_difficulty;

    // For target adjustment, always divide first then multiply
    let new_target = (target_current / scale) * adjustment_ratio;

    max_u256 - new_target
}

pub struct AdjustmentStats {
    pub actual_block_time: Duration,
    pub target_block_time: Duration,
    pub percent_different: u32,
    pub min_threshold: u32,
    pub is_adjusted: bool,
}
pub fn calculate_difficulty(
    block_height: u64,
    last_diff_timestamp: u128,
    current_timestamp: u128,
    current_difficulty: U256,
    difficulty_config: &DifficultyAdjustmentConfig,
) -> (U256, Option<AdjustmentStats>) {
    let blocks_between_adjustments = difficulty_config.adjustment_interval as u128;

    // Early return if no difficulty adjustment needed
    if block_height as u128 % blocks_between_adjustments != 0 {
        return (current_difficulty, None);
    }

    // Calculate times
    let target_block_time_ms = (difficulty_config.target_block_time * 1000) as u128;
    let target_time_ms = target_block_time_ms * blocks_between_adjustments;
    let actual_time_ms = current_timestamp - last_diff_timestamp;

    let actual_block_time =
        Duration::from_millis((actual_time_ms / blocks_between_adjustments) as u64);
    let target_block_time =
        Duration::from_millis((target_time_ms / blocks_between_adjustments) as u64);

    // Calculate difference
    let percent_diff = actual_block_time.abs_diff(target_block_time).as_millis() * 100
        / target_block_time.as_millis();
    let min_threshold = (difficulty_config.min_adjustment_factor * 100.0) as u128;

    let stats = AdjustmentStats {
        actual_block_time,
        target_block_time,
        percent_different: percent_diff as u32,
        min_threshold: min_threshold as u32,
        is_adjusted: percent_diff > min_threshold,
    };

    let difficulty = if stats.is_adjusted {
        adjust_difficulty(current_difficulty, actual_time_ms, target_time_ms)
    } else {
        current_difficulty
    };

    (difficulty, Some(stats))
}
//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::Address;
    use openssl::sha;

    use crate::{
        adjust_difficulty, calculate_initial_difficulty, StorageConfig, H256, PACKING_SHA_1_5_S,
        U256,
    };

    use super::DifficultyAdjustmentConfig;

    #[test]
    fn test_adjustments() {
        let difficulty_config = DifficultyAdjustmentConfig {
            target_block_time: 5,        // 5 seconds
            adjustment_interval: 10,     // every X blocks
            max_adjustment_factor: 4,    // No more than 4x or 1/4th with each adjustment
            min_adjustment_factor: 0.25, // a minimum 25% adjustment threshold
            min_difficulty: U256::one(),
            max_difficulty: U256::MAX,
        };

        let storage_config = StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 40,
            num_chunks_in_recall_range: 8,
            num_partitions_in_slot: 1,
            miner_address: Address::random(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: PACKING_SHA_1_5_S,
        };

        let mut storage_module_count = 3;

        let seed = hash_sha256("test".as_bytes());
        let hashes_per_second = storage_config.num_chunks_in_recall_range * storage_module_count;

        let difficulty =
            calculate_initial_difficulty(&difficulty_config, &storage_config, storage_module_count)
                .unwrap();

        let num_blocks = 2000;
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);

        let expected = difficulty_config.target_block_time as f64;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);
        println!(" block time: {:.2?}", seconds_to_duration(block_time));

        // Lets increase the hashrate by 2x so blocks are coming too quickly
        println!("Double the hash power and verify block_times are half as long");
        storage_module_count = 6;
        let hashes_per_second = storage_config.num_chunks_in_recall_range * storage_module_count;
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(block_time));

        let expected = 2.5; // with 2x the hash power we expect 1/2 the block time.
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Perform a difficulty adjustment with the new block_time");
        let target_time_ms = (difficulty_config.target_block_time * 1000) as u128;
        let actual_time_ms = (block_time * 1000.0) as u128;
        let difficulty = adjust_difficulty(difficulty, actual_time_ms, target_time_ms);
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(block_time));

        let expected = 5.0; // Expect the difficulty to adjust back to 5s blocks
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Double the hashpower again and expect block_time to half");
        storage_module_count = 12;
        let hashes_per_second = storage_config.num_chunks_in_recall_range * storage_module_count;
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(new_block_time));

        let expected = 2.5; // with 2x the hash power we expect roughly 1/2 the block time.
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        // The adjustment has over corrected, let it adjust again
        println!("Adjust difficulty to account for hashpower doubling");
        let target_time_ms = (difficulty_config.target_block_time * 1000) as u128;
        let actual_time_ms = (new_block_time * 1000.0) as u128;
        let difficulty = adjust_difficulty(difficulty, actual_time_ms, target_time_ms);
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(new_block_time));

        let expected = 5.0; // Expect the difficulty to adjust back to 5s blocks
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Reduce hashpower to 1/4th of previous");
        storage_module_count = 3;
        let hashes_per_second = storage_config.num_chunks_in_recall_range * storage_module_count;
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(new_block_time));

        let expected = 20.0; // with 1/4th the hashpower we'd expect 4x block times
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Apply difficulty adjustment");
        let target_time_ms = (difficulty_config.target_block_time * 1000) as u128;
        let actual_time_ms = (new_block_time * 1000.0) as u128;
        let difficulty = adjust_difficulty(difficulty, actual_time_ms, target_time_ms);
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(block_time));

        let expected = 5.0;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Apply difficulty adjustment");
        let target_time_ms = (difficulty_config.target_block_time * 1000) as u128;
        let actual_time_ms = (block_time * 1000.0) as u128;
        let difficulty = adjust_difficulty(difficulty, actual_time_ms, target_time_ms);
        let (mean, _seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", seconds_to_duration(mean));

        let expected = 5.0;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);
    }

    fn assert_expected_with_tolerance(expected: f64, actual: f64, tolerance: f64) {
        let abs_difference = (expected - actual).abs();
        assert!(
            abs_difference <= tolerance,
            "Difference {} exceeds tolerance {}",
            abs_difference,
            tolerance
        );
    }

    fn seconds_to_duration(seconds: f64) -> Duration {
        Duration::from_nanos((seconds * 1_000_000_000.0) as u64)
    }

    fn one_second_of_hashes(
        hashes_per_second: u64,
        initial_hash: H256,
        difficulty: U256,
    ) -> ((bool, H256), f64) {
        let mut prev_hash = initial_hash;
        for i in 0..hashes_per_second {
            prev_hash = hash_sha256(&prev_hash.0);
            let hash_val = hash_to_number(&prev_hash.0);
            if hash_val >= difficulty {
                return ((true, prev_hash), i as f64 / hashes_per_second as f64);
            }
        }
        ((false, prev_hash), 1.0)
    }

    /// SHA256 hash the message parameter
    fn hash_sha256(message: &[u8]) -> H256 {
        let mut hasher = sha::Sha256::new();
        hasher.update(message);
        H256::from(hasher.finish())
    }

    fn hash_to_number(hash: &[u8]) -> U256 {
        U256::from_little_endian(hash)
    }

    fn mine_block(hashes_per_second: u64, seed: H256, difficulty: U256) -> (f64, H256) {
        let mut num_seconds: f64 = 0.0;
        let mut solution_found = false;
        let mut initial_hash = seed;

        while !solution_found {
            let ((sf, seed), duration) =
                one_second_of_hashes(hashes_per_second, initial_hash, difficulty);
            num_seconds += duration;
            solution_found = sf;
            initial_hash = seed;
        }

        (num_seconds, initial_hash)
    }

    fn simulate_mining(
        num_blocks: u64,
        hashes_per_second: u64,
        seed: H256,
        difficulty: U256,
    ) -> (f64, H256) {
        println!(" mining {} blocks...", num_blocks);
        // Mine num_blocks an record their block times
        let mut block_times: Vec<f64> = Vec::new();
        let mut internal_seed = seed;
        for _ in 0..num_blocks {
            let (num_seconds, s) = mine_block(hashes_per_second, internal_seed, difficulty);
            block_times.push(num_seconds);
            internal_seed = s;
        }

        // Calculate the mean block time to see if we're in the ball park
        let mean = if block_times.is_empty() {
            0.0
        } else {
            block_times.iter().sum::<f64>() / block_times.len() as f64
        };
        (mean, internal_seed)
    }
}
