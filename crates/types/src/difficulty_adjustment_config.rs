use std::time::Duration;
use tracing::info;

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

// let block_hashrate = max_difficulty / (max_difficulty - difficulty);
pub fn get_initial_difficulty(
    difficulty_config: &DifficultyAdjustmentConfig,
    storage_config: &StorageConfig,
    storage_module_count: u64,
) -> U256 {
    let hashes_per_partition_per_second = storage_config.num_chunks_in_recall_range;
    let node_hashes_per_second = U256::from(hashes_per_partition_per_second * storage_module_count);
    let block_hashrate = node_hashes_per_second * (difficulty_config.target_block_time);

    // Compute the expected hash rate
    if block_hashrate == U256::zero() {
        panic!("block_hashrate cannot be zero");
    }

    let max_difficulty = difficulty_config.max_difficulty;
    let initial_difficulty = max_difficulty - (max_difficulty / block_hashrate);
    initial_difficulty
}
pub struct TimeStats {
    actual_mean: Duration,
    target_mean: Duration,
    percent_change: u128,
    direction: (&'static str, &'static str),
}

pub fn compute_difficulty_adjustment(
    block_height: u64,
    current_timestamp: u128,
    last_diff_timestamp: u128,
    last_difficulty: U256,
    config: &DifficultyAdjustmentConfig,
) -> Option<U256> {
    if block_height % config.adjustment_interval as u64 != 0 {
        return None;
    }

    let time_stats = calculate_time_stats(current_timestamp, last_diff_timestamp, config);

    // Log status and check if adjustment needed
    let min_threshold = (config.min_adjustment_factor * 1000.0) as u128;
    if time_stats.percent_change <= min_threshold {
        println!(
            "\n🧊 Block time {:?} is {:.2}% {} than target {:?}, within threshold - no adjustment",
            time_stats.actual_mean,
            time_stats.percent_change as f64 / 10.0,
            time_stats.direction.0,
            time_stats.target_mean
        );
        return None;
    }

    println!(
        "\n🧊 Block time {:?} is {:.2}% {} than target {:?}, adjusting {}",
        time_stats.actual_mean,
        time_stats.percent_change as f64 / 10.0,
        time_stats.direction.0,
        time_stats.target_mean,
        time_stats.direction.1
    );

    // Calculate and bound new difficulty
    let scale = U256::from(1000);
    let actual_time = time_stats.actual_mean.as_secs();
    let target_time = time_stats.target_mean.as_secs();
    // Pre-divide by scale to avoid overflow when doing max_diff - last_difficulty
    let diff_inverse = (U256::MAX / scale - last_difficulty / scale)  // Room to adjust, scaled down
        * actual_time                                                       // Multiply by time ratio
        / target_time; // Complete the ratio
    let new_diff = U256::MAX - diff_inverse * scale; // Scale back up for final result

    // Apply bounds
    let max_factor = U256::from(config.max_adjustment_factor);
    let bounded_diff = if new_diff > last_difficulty {
        if new_diff / last_difficulty >= max_factor {
            info!("Capping difficulty increase at {}x", max_factor);
            last_difficulty * max_factor
        } else {
            new_diff
        }
    } else if last_difficulty / new_diff >= max_factor {
        info!("Capping difficulty decrease at 1/{}", max_factor);
        last_difficulty / max_factor
    } else {
        new_diff
    };

    println!(
        " max: {}\nlast: {}\nnext: {}",
        U256::MAX,
        last_difficulty,
        bounded_diff
    );

    Some(bounded_diff)
}

fn calculate_time_stats(
    current_ts: u128,
    last_ts: u128,
    config: &DifficultyAdjustmentConfig,
) -> TimeStats {
    let interval = config.adjustment_interval as u32;
    let target = Duration::from_secs(config.target_block_time) * interval;
    let actual = Duration::from_millis((current_ts - last_ts) as u64);

    TimeStats {
        actual_mean: actual / interval,
        target_mean: target / interval,
        percent_change: (actual.abs_diff(target).as_millis() * 1000) / target.as_millis(),
        direction: if actual / interval >= target / interval {
            ("greater", "DOWN to shorten block times")
        } else {
            ("less", "UP to lengthen block times")
        },
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use crate::{
        compute_difficulty_adjustment, get_initial_difficulty, StorageConfig, PACKING_SHA_1_5_S,
        U256,
    };

    use super::DifficultyAdjustmentConfig;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_adjustments() {
        let mut difficulty_config = DifficultyAdjustmentConfig {
            target_block_time: 5,        // 5 seconds
            adjustment_interval: 10,     // every X blocks
            max_adjustment_factor: 4,    // No more than 4x or 1/4th with each adjustment
            min_adjustment_factor: 0.25, // a minimum 25% adjustment threshold
            min_difficulty: U256::one(),
            max_difficulty: U256::MAX,
        };

        let storage_config = StorageConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: Address::random(),
            min_writes_before_sync: 1,
            entropy_packing_iterations: PACKING_SHA_1_5_S,
        };

        let prev_diff = get_initial_difficulty(&difficulty_config, &storage_config, 3);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let current_timestamp = now.as_millis();

        // Compute a timestamp that will result in blocks being 50% shorter than they should be
        let target_time =
            difficulty_config.adjustment_interval * difficulty_config.target_block_time;
        let actual_time = target_time / 2;
        let last_diff_timestamp = current_timestamp - (actual_time * 1000) as u128;

        let new_diff = compute_difficulty_adjustment(
            10,
            current_timestamp,
            last_diff_timestamp,
            prev_diff,
            &difficulty_config,
        )
        .unwrap();

        let delta_diff = new_diff - prev_diff;
        let scaled_diff = prev_diff / 2;

        println!(
            " prev_diff: {}\n  new_diff: {}\ndelta_diff: {}\nscale_diff: {}",
            prev_diff, new_diff, delta_diff, scaled_diff
        );

        let guess_diff = prev_diff + (delta_diff / 2);
        println!("guess_diff: {}", guess_diff);
        println!("delta %: {}", ((delta_diff * 1000) / U256::MAX) / 1000);
    }
}
