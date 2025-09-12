use std::time::Duration;

use crate::{ConsensusConfig, DifficultyAdjustmentConfig, U256};
use rust_decimal_macros::dec;

pub fn calculate_initial_difficulty(
    consensus_config: &ConsensusConfig,
    storage_module_count: u64,
) -> eyre::Result<U256> {
    let hashes_per_sec = consensus_config.num_chunks_in_recall_range * storage_module_count;
    let block_time = consensus_config.difficulty_adjustment.block_time;

    eyre::ensure!(
        !(hashes_per_sec == 0 || block_time == 0),
        "Input values cannot be zero"
    );

    let max_diff = U256::MAX;
    let block_hashrate = U256::from(hashes_per_sec) * U256::from(block_time);

    let initial_difficulty = max_diff - (max_diff / block_hashrate);
    let target = max_diff - initial_difficulty;

    // Rearranged operations to avoid overflow
    let scale = U256::from(1_000_000);
    let probability_per_hash = target / (max_diff / scale); // Divide max first
    let expected_hashes = scale / probability_per_hash;

    tracing::info!("Block hashrate: {}", block_hashrate);
    tracing::info!("Initial difficulty: {}", initial_difficulty);
    tracing::info!("Target: {}", target);
    tracing::info!("Probability per hash (×10^-6): {}", probability_per_hash);
    tracing::info!("Expected hashes to find block: {}", expected_hashes);

    Ok(initial_difficulty)
}

/// Adjusts mining difficulty based on actual vs target block time.
/// - if `actual_time_ms` < `target_time_ms`, the difficulty increases i.e. block.difficulty > previous_block.difficulty.
/// - if `actual_time_ms` > `target_time_ms`, the difficulty decreases i.e. block.difficulty < previous_block.difficulty.
/// - if the `percent_diff` < `min_threshold`, the difficulty remains unchanged.
pub fn adjust_difficulty(
    current_diff: U256,
    actual_time_ms: u128,
    target_time_ms: u128,
    max_adjustment_threshold: u128,
) -> U256 {
    assert!(target_time_ms != 0, "target_time_ms must be > 0");

    let max_u256 = U256::MAX;

    // Uses a scale factor of 1000 to preserve fractional precision during integer arithmetic.
    let scale = U256::from(1000);

    // Calculate the raw adjustment ratio
    let raw_adjustment_ratio = (U256::from(actual_time_ms) * scale) / U256::from(target_time_ms);

    // Convert max_adjustment_threshold (percentage) to ratio bounds
    // e.g., if max_adjustment_threshold = 400 (400%), then max_ratio = 4 * scale = 4000
    let max_ratio = U256::from(max_adjustment_threshold) * scale / U256::from(100);
    let min_ratio = scale * scale / max_ratio; // Reciprocal: 1000*1000 / 4000 = 250 (0.25 * scale)

    // Clamp the adjustment ratio to be within [min_ratio, max_ratio]
    let adjustment_ratio = raw_adjustment_ratio.min(max_ratio).max(min_ratio);

    let target_current = max_u256.saturating_sub(current_diff);

    // Use saturating_mul to prevent overflow - clamps to U256::MAX if overflow would occur
    let new_target = (target_current / scale).saturating_mul(adjustment_ratio);

    max_u256.saturating_sub(new_target)
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
    current_diff: U256,
    difficulty_config: &DifficultyAdjustmentConfig,
) -> (U256, Option<AdjustmentStats>) {
    let blocks_between_adjustments = difficulty_config.difficulty_adjustment_interval as u128;

    // Early return if no difficulty adjustment needed
    if block_height as u128 % blocks_between_adjustments != 0 {
        return (current_diff, None);
    }

    // Calculate times
    let target_block_time_ms = (difficulty_config.block_time * 1000) as u128;
    let target_time_ms = target_block_time_ms * blocks_between_adjustments;
    let actual_time_ms = current_timestamp - last_diff_timestamp;

    let actual_block_time =
        Duration::from_millis((actual_time_ms / blocks_between_adjustments) as u64);
    let target_block_time =
        Duration::from_millis((target_time_ms / blocks_between_adjustments) as u64);

    // Calculate percentage difference
    let percent_diff = if actual_block_time > target_block_time {
        // Blocks taking longer than target (slow blocks)
        ((actual_block_time.as_millis() - target_block_time.as_millis()) * 100)
            / target_block_time.as_millis()
    } else {
        // Blocks coming faster than target (fast blocks)
        ((target_block_time.as_millis() - actual_block_time.as_millis()) * 100)
            / target_block_time.as_millis()
    };

    let min_threshold: u128 = (difficulty_config.min_difficulty_adjustment_factor * dec![100.0])
        .try_into()
        .unwrap();

    // Max threshold to clamp difficulty change
    let max_adjustment_threshold: u128 = (difficulty_config.max_difficulty_adjustment_factor
        * dec![100.0])
    .try_into()
    .unwrap();

    let is_adjusted = percent_diff >= min_threshold;

    let stats = AdjustmentStats {
        actual_block_time,
        target_block_time,
        percent_different: percent_diff as u32,
        min_threshold: min_threshold.try_into().expect("Value exceeds u32::MAX"),
        is_adjusted,
    };

    let difficulty = if stats.is_adjusted {
        adjust_difficulty(
            current_diff,
            actual_time_ms,
            target_time_ms,
            max_adjustment_threshold,
        )
    } else {
        current_diff
    };

    (difficulty, Some(stats))
}

/// Calculates the next cumulative difficulty by adding the expected hashes needed
/// (max_diff / (max_diff - new_diff)) to the previous cumulative difficulty.
pub fn next_cumulative_diff(previous_cumulative_diff: U256, new_diff: U256) -> U256 {
    let max_diff = U256::MAX;
    let network_hash_rate = max_diff / (max_diff - new_diff);
    previous_cumulative_diff + network_hash_rate
}

#[cfg(test)]
#[expect(
    clippy::float_arithmetic,
    reason = "tests use float seconds; production code remains integer/fixed-point"
)]
mod tests {
    use super::DifficultyAdjustmentConfig;
    use super::*;
    use crate::{
        adjust_difficulty, calculate_difficulty, calculate_initial_difficulty, H256, U256,
    };
    use openssl::sha;
    use rstest::{fixture, rstest};
    use std::time::Duration;

    #[fixture]
    fn default_difficulty_config() -> DifficultyAdjustmentConfig {
        DifficultyAdjustmentConfig {
            block_time: 10,
            difficulty_adjustment_interval: 100,
            max_difficulty_adjustment_factor: dec![4.0],
            min_difficulty_adjustment_factor: dec![0.25],
        }
    }

    #[test]
    fn test_adjustments() {
        let mut consensus_config = ConsensusConfig::testing();
        consensus_config.difficulty_adjustment = DifficultyAdjustmentConfig {
            block_time: 5,
            difficulty_adjustment_interval: 10,
            max_difficulty_adjustment_factor: dec![4],
            min_difficulty_adjustment_factor: dec![0.25],
        };
        consensus_config.chunk_size = 32;
        consensus_config.num_chunks_in_partition = 40;
        consensus_config.num_chunks_in_recall_range = 8;
        consensus_config.num_partitions_per_slot = 1;
        consensus_config.block_migration_depth = 1; // Testnet / single node config

        let mut storage_module_count = 3;

        let seed = hash_sha256("test".as_bytes());
        let hashes_per_second = consensus_config.num_chunks_in_recall_range * storage_module_count;

        let difficulty =
            calculate_initial_difficulty(&consensus_config, storage_module_count).unwrap();

        let num_blocks = 2000;
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);

        let expected = consensus_config.difficulty_adjustment.block_time as f64;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);
        println!(" block time: {:.2?}", Duration::from_secs_f64(block_time));

        // Lets increase the hashrate by 2x so blocks are coming too quickly
        println!("Double the hash power and verify block_times are half as long");
        storage_module_count = 6;
        let hashes_per_second = consensus_config.num_chunks_in_recall_range * storage_module_count;
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", Duration::from_secs_f64(block_time));

        let expected = 2.5; // with 2x the hash power we expect 1/2 the block time.
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Perform a difficulty adjustment with the new block_time");
        let target_time_ms = (consensus_config.difficulty_adjustment.block_time * 1000) as u128;
        let actual_time_ms = (block_time * 1000.0) as u128;
        let max_threshold = (consensus_config
            .difficulty_adjustment
            .max_difficulty_adjustment_factor
            * dec![100.0])
        .try_into()
        .unwrap();
        let difficulty =
            adjust_difficulty(difficulty, actual_time_ms, target_time_ms, max_threshold);
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", Duration::from_secs_f64(block_time));

        let expected = 5.0; // Expect the difficulty to adjust back to 5s blocks
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Double the hashpower again and expect block_time to half");
        storage_module_count = 12;
        let hashes_per_second = consensus_config.num_chunks_in_recall_range * storage_module_count;
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(
            " block time: {:.2?}",
            Duration::from_secs_f64(new_block_time)
        );

        let expected = 2.5; // with 2x the hash power we expect roughly 1/2 the block time.
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        // The adjustment has over corrected, let it adjust again
        println!("Adjust difficulty to account for hashpower doubling");
        let target_time_ms = (consensus_config.difficulty_adjustment.block_time * 1000) as u128;
        let actual_time_ms = (new_block_time * 1000.0) as u128;
        let max_threshold = (consensus_config
            .difficulty_adjustment
            .max_difficulty_adjustment_factor
            * dec![100.0])
        .try_into()
        .unwrap();
        let difficulty =
            adjust_difficulty(difficulty, actual_time_ms, target_time_ms, max_threshold);
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(
            " block time: {:.2?}",
            Duration::from_secs_f64(new_block_time)
        );

        let expected = 5.0; // Expect the difficulty to adjust back to 5s blocks
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Reduce hashpower to 1/4th of previous");
        storage_module_count = 7;
        let hashes_per_second = consensus_config.num_chunks_in_recall_range * storage_module_count;
        let (new_block_time, seed) =
            simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(
            " block time: {:.2?}",
            Duration::from_secs_f64(new_block_time)
        );

        let expected = 8.33; // with 60% of the hashpower we'd expect 1.667x the block times
        let actual = new_block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Apply difficulty adjustment");
        let target_time_ms = (consensus_config.difficulty_adjustment.block_time * 1000) as u128;
        let actual_time_ms = (new_block_time * 1000.0) as u128;
        let max_threshold = (consensus_config
            .difficulty_adjustment
            .max_difficulty_adjustment_factor
            * dec![100.0])
        .try_into()
        .unwrap();
        let difficulty =
            adjust_difficulty(difficulty, actual_time_ms, target_time_ms, max_threshold);
        let (block_time, seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", Duration::from_secs_f64(block_time));

        let expected = 5.0;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);

        println!("Apply difficulty adjustment");
        let target_time_ms = (consensus_config.difficulty_adjustment.block_time * 1000) as u128;
        let actual_time_ms = (block_time * 1000.0) as u128;
        let max_threshold = (consensus_config
            .difficulty_adjustment
            .max_difficulty_adjustment_factor
            * dec![100.0])
        .try_into()
        .unwrap();
        let difficulty =
            adjust_difficulty(difficulty, actual_time_ms, target_time_ms, max_threshold);
        let (mean, _seed) = simulate_mining(num_blocks, hashes_per_second, seed, difficulty);
        println!(" block time: {:.2?}", Duration::from_secs_f64(mean));

        let expected = 5.0;
        let actual = block_time;
        assert_expected_with_tolerance(expected, actual, 1.0);
    }

    fn assert_expected_with_tolerance(expected: f64, actual: f64, tolerance: f64) {
        // Compare using integer milliseconds only:
        // Convert the provided seconds (expected/actual/tolerance) to integer milliseconds by rounding.
        let expected_ms = (expected * 1000.0).round() as i64;
        let actual_ms = (actual * 1000.0).round() as i64;
        let tolerance_ms = (tolerance * 1000.0).round() as i64;
        let diff_ms = (expected_ms - actual_ms).abs();
        assert!(
            diff_ms <= tolerance_ms,
            "Difference {}ms exceeds tolerance {}ms",
            diff_ms,
            tolerance_ms
        );
    }

    fn one_second_of_hashes(
        hashes_per_second: u64,
        initial_hash: H256,
        difficulty: U256,
    ) -> ((bool, H256), u64) {
        let mut prev_hash = initial_hash;
        for i in 0..hashes_per_second {
            prev_hash = hash_sha256(&prev_hash.0);
            let hash_val = hash_to_number(&prev_hash.0);
            if hash_val >= difficulty {
                // Return elapsed time within this one-second bucket in milliseconds.
                // i hashes out of hashes_per_second ⇒ elapsed_ms = floor(i * 1000 / hashes_per_second)
                let elapsed_ms = if hashes_per_second == 0 {
                    1000
                } else {
                    ((i as u128) * 1000_u128 / (hashes_per_second as u128)) as u64
                };
                return ((true, prev_hash), elapsed_ms);
            }
        }
        ((false, prev_hash), 1000)
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
        // Accumulate integer milliseconds, convert to seconds (f64) once at the end.
        let mut elapsed_ms: u128 = 0;
        let mut solution_found = false;
        let mut initial_hash = seed;

        while !solution_found {
            let ((sf, seed), duration_ms) =
                one_second_of_hashes(hashes_per_second, initial_hash, difficulty);
            elapsed_ms += duration_ms as u128;
            solution_found = sf;
            initial_hash = seed;
        }

        (elapsed_ms as f64 / 1000.0, initial_hash)
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
            // Sum using integer milliseconds to avoid repeated float arithmetic, then convert once.
            let total_ms: u128 = block_times
                .iter()
                .map(|s| (s * 1000.0).round() as u128)
                .sum();
            (total_ms as f64 / 1000.0) / (block_times.len() as f64)
        };
        (mean, internal_seed)
    }

    #[rstest]
    // Below min threshold (25%) - no adjustment
    #[case(1.05, 5, false, false)] // 105% of target = 5% diff
    #[case(1.10, 10, false, false)] // 110% of target = 10% diff
    #[case(1.20, 20, false, false)] // 120% of target = 20% diff
    #[case(0.95, 5, false, false)] // 95% of target = 5% diff
    #[case(0.90, 10, false, false)]
    // 90% of target = 10% diff

    // At min threshold boundary (25%) - adjustment happens (>= threshold)
    #[case(1.25, 25, true, false)] // 125% of target = 25% diff - blocks too slow, decrease difficulty
    #[case(0.75, 25, true, true)]
    // 75% of target = 25% diff - blocks too fast, increase difficulty

    // Above min threshold - adjustment happens
    #[case(1.3, 30, true, false)] // 130% of target = 30% diff, decrease
    #[case(1.5, 50, true, false)] // 150% of target = 50% diff, decrease
    #[case(2.0, 100, true, false)] // 200% of target = 100% diff, decrease
    #[case(0.7, 30, true, true)] // 70% of target = 30% diff, increase
    #[case(0.5, 50, true, true)] // 50% of target = 50% diff, increase
    #[case(0.4, 60, true, true)]
    // 40% of target = 60% diff, increase

    // Large changes - adjustment happens but may be clamped by max threshold
    #[case(3.0, 200, true, false)] // 300% of target = 200% diff, decrease (may be clamped)
    #[case(4.0, 300, true, false)] // 400% of target = 300% diff, decrease (may be clamped)
    #[case(5.0, 400, true, false)] // 500% of target = 400% diff, decrease (may be clamped)
    #[case(0.3, 70, true, true)] // 30% of target = 70% diff, increase (may be clamped)
    #[case(0.2, 80, true, true)]
    // 20% of target = 80% diff, increase (may be clamped)

    // Very large changes - adjustment happens but definitely clamped
    #[case(6.0, 500, true, false)] // 600% of target = 500% diff, decrease (will be clamped)
    #[case(0.16, 84, true, true)] // 16% of target = 84% diff, increase (will be clamped)
    fn test_difficulty_thresholds_comprehensive(
        default_difficulty_config: DifficultyAdjustmentConfig,
        #[case] time_multiplier: f64,
        #[case] expected_percent: u32,
        #[case] should_adjust: bool,
        #[case] should_increase: bool,
    ) {
        let difficulty_config = default_difficulty_config;
        let block_height = 100; // Adjustment block
        let current_diff = U256::from(1000000_u64);
        let last_diff_timestamp = 0_u128;

        let blocks = difficulty_config.difficulty_adjustment_interval as u128;
        let target_time = difficulty_config.block_time as u128 * 1000 * blocks;
        // Compute actual_time in integer milliseconds from multiplier ratio (e.g., 1.25 -> +25%)
        // Convert multiplier to percent as an integer to avoid float arithmetic
        let multiplier_percent = (time_multiplier * 100.0).round() as u128;
        let actual_time = target_time * multiplier_percent / 100;
        let current_timestamp = last_diff_timestamp + actual_time;

        let (new_diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_diff,
            &difficulty_config,
        );

        assert!(
            stats.is_some(),
            "Stats should always be Some at adjustment block"
        );
        let stats = stats.unwrap();

        // Verify adjustment status
        assert_eq!(
            stats.is_adjusted, should_adjust,
            "Adjustment mismatch for {}% difference (time_multiplier: {})",
            expected_percent, time_multiplier
        );

        // Verify percent calculation
        assert_eq!(
            stats.percent_different, expected_percent,
            "Percent difference mismatch"
        );

        // Verify difficulty change
        if should_adjust {
            assert_ne!(
                new_diff, current_diff,
                "Difficulty should change when adjusted"
            );

            // Verify direction of change
            if should_increase {
                assert!(
                    new_diff > current_diff,
                    "Difficulty should increase (blocks too fast)"
                );
            } else {
                assert!(
                    new_diff < current_diff,
                    "Difficulty should decrease (blocks too slow)"
                );
            }
        } else {
            assert_eq!(new_diff, current_diff, "Difficulty should not change");
        }
    }

    #[rstest]
    fn test_boundary_condition_exact_threshold(
        default_difficulty_config: DifficultyAdjustmentConfig,
    ) {
        // Test the specific boundary fix: percent_diff >= min_threshold vs percent_diff > min_threshold
        let difficulty_config = default_difficulty_config;
        let block_height = 100; // Adjustment block
        let current_diff = U256::from(1000000_u64);
        let last_diff_timestamp = 0_u128;

        let blocks = difficulty_config.difficulty_adjustment_interval as u128;
        let target_time = difficulty_config.block_time as u128 * 1000 * blocks;

        // Create scenario where percent_diff exactly equals min_threshold (25%)
        let actual_time = target_time + (target_time / 4); // 125% of target = 25% diff
        let current_timestamp = last_diff_timestamp + actual_time;

        let (new_diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_diff,
            &difficulty_config,
        );

        let stats = stats.expect("Stats should be Some at adjustment block");

        // Exactly at threshold should trigger adjustment with >= comparison
        assert_eq!(
            stats.percent_different, 25,
            "Should calculate 25% difference"
        );
        assert_eq!(stats.min_threshold, 25, "Min threshold should be 25%");
        assert!(
            stats.is_adjusted,
            "Should adjust when percent_diff == min_threshold"
        );
        assert_ne!(
            new_diff, current_diff,
            "Difficulty should change at exact threshold"
        );
        assert!(
            new_diff < current_diff,
            "Difficulty should decrease (blocks too slow)"
        );
    }

    #[rstest]
    fn test_boundary_condition_just_below_threshold(
        default_difficulty_config: DifficultyAdjustmentConfig,
    ) {
        let difficulty_config = default_difficulty_config;
        let block_height = 100;
        let current_diff = U256::from(1000000_u64);
        let last_diff_timestamp = 0_u128;

        let blocks = difficulty_config.difficulty_adjustment_interval as u128;
        let target_time = difficulty_config.block_time as u128 * 1000 * blocks;

        // Create scenario just below threshold (24% diff)
        let actual_time = target_time + (target_time * 24 / 100); // 124% of target = 24% diff
        let current_timestamp = last_diff_timestamp + actual_time;

        let (new_diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_diff,
            &difficulty_config,
        );

        let stats = stats.expect("Stats should be Some at adjustment block");

        assert_eq!(
            stats.percent_different, 24,
            "Should calculate 24% difference"
        );
        assert!(!stats.is_adjusted, "Should NOT adjust when below threshold");
        assert_eq!(new_diff, current_diff, "Difficulty should remain unchanged");
    }

    #[rstest]
    fn test_boundary_condition_just_above_threshold(
        default_difficulty_config: DifficultyAdjustmentConfig,
    ) {
        let difficulty_config = default_difficulty_config;
        let block_height = 100;
        let current_diff = U256::from(1000000_u64);
        let last_diff_timestamp = 0_u128;

        let blocks = difficulty_config.difficulty_adjustment_interval as u128;
        let target_time = difficulty_config.block_time as u128 * 1000 * blocks;

        // Create scenario just above threshold (26% diff)
        let actual_time = target_time + (target_time * 26 / 100); // 126% of target = 26% diff
        let current_timestamp = last_diff_timestamp + actual_time;

        let (new_diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_diff,
            &difficulty_config,
        );

        let stats = stats.expect("Stats should be Some at adjustment block");

        assert_eq!(
            stats.percent_different, 26,
            "Should calculate 26% difference"
        );
        assert!(stats.is_adjusted, "Should adjust when above threshold");
        assert_ne!(
            new_diff, current_diff,
            "Difficulty should change above threshold"
        );
    }

    #[rstest]
    #[case(0.75)] // Exactly at 25% threshold (blocks 25% faster than target)
    #[case(0.76)] // Just above threshold (24% faster, should NOT adjust)
    #[case(0.74)] // Just below threshold (26% faster, should adjust)
    fn test_fast_blocks_boundary_conditions(
        default_difficulty_config: DifficultyAdjustmentConfig,
        #[case] time_fraction: f64,
    ) {
        let difficulty_config = default_difficulty_config;
        let block_height = 100;
        let current_diff = U256::from(1000000_u64);
        let last_diff_timestamp = 0_u128;

        let blocks = difficulty_config.difficulty_adjustment_interval as u128;
        let target_time = difficulty_config.block_time as u128 * 1000 * blocks;

        // Fast blocks: actual_time < target_time
        let actual_time = (target_time as f64 * time_fraction) as u128;
        let current_timestamp = last_diff_timestamp + actual_time;

        let (new_diff, stats) = calculate_difficulty(
            block_height,
            last_diff_timestamp,
            current_timestamp,
            current_diff,
            &difficulty_config,
        );

        let stats = stats.expect("Stats should be Some at adjustment block");

        let expected_percent = ((1.0 - time_fraction) * 100.0) as u32;
        assert_eq!(
            stats.percent_different, expected_percent,
            "Percent calculation should be correct"
        );

        if time_fraction <= 0.75 {
            // 25% or more difference
            assert!(
                stats.is_adjusted,
                "Should adjust at/above 25% threshold for fast blocks"
            );
            assert!(
                new_diff > current_diff,
                "Difficulty should increase (blocks too fast)"
            );
        } else {
            assert!(!stats.is_adjusted, "Should NOT adjust below 25% threshold");
            assert_eq!(new_diff, current_diff, "Difficulty should remain unchanged");
        }
    }
}
