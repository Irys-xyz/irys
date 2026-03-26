use eyre::Result;
use irys_types::{ConsensusConfig, H256List, SimpleRNG, H256};
use openssl::sha;
use std::collections::HashMap;
use tracing::{debug, info};

/// number of vdf steps cached for efficient sampling after ranges reinitialization
pub const NUMBER_OF_KEPT_LAST_STEPS: u64 = 20;
/// Efficient sampling: randomly picks partition ranges indexes in [0..NUM_RECALL_RANGES_IN_PARTITION-1] interval without repeating up to automatic reinitialization after all indexes are retrieved.
#[derive(Debug, Clone)]
pub struct Ranges {
    /// Available partition's ranges indexes
    ranges: Vec<usize>,
    /// last valid range position
    last_range_pos: usize,
    /// last step number
    pub last_step_num: u64,
    /// last recall ranges by step number
    last_recall_ranges: HashMap<u64, usize>,
    /// num recall ranges in a partition, equal to ranges vector capacity
    pub num_recall_ranges_in_partition: usize,
}

impl Ranges {
    /// Returns recall range index for a given step number, seed and partition hash.
    /// if the range is already cached, it returns the cached range, otherwise it picks a new random range.
    pub fn get_recall_range(
        &mut self,
        step: u64,
        seed: &H256,
        partition_hash: &H256,
    ) -> Result<usize> {
        if let Some(&range) = self.last_recall_ranges.get(&step) {
            debug!(
                "Partition hash {}, Recall range for step {} is cached, range {}/{}",
                partition_hash, step, range, self.num_recall_ranges_in_partition
            );
            return Ok(range);
        };

        let range = self.next_recall_range(step, seed, partition_hash)?;
        debug!("Partition hash {}, Recall range for step {} is not cached, calling next range, range {}/{}", partition_hash, step, range, self.num_recall_ranges_in_partition);
        Ok(range)
    }

    pub fn get_last_recall_range(self) -> Option<usize> {
        self.last_recall_ranges.get(&self.last_step_num).copied()
    }

    /// Picks next random (using seed as entropy) range idx in [0..NUM_RECALL_RANGES_IN_PARTITION-1] interval
    pub fn next_recall_range(
        &mut self,
        step: u64,
        seed: &H256,
        partition_hash: &H256,
    ) -> Result<usize> {
        // non consecutive vdf_steps is handled at mining level
        if step != self.last_step_num + 1 {
            return Err(eyre::eyre!(
                "Non consecutive vdf steps are not supported, last step num {}, current step num {}",
                self.last_step_num,
                step
            ));
        }

        let range = if self.last_range_pos == 0 {
            let range = self.ranges[0];
            self.reinitialize();
            range
        } else {
            let mut hasher = sha::Sha256::new();
            hasher.update(&seed.0);
            hasher.update(&partition_hash.0);
            let rng_seed: u32 = u32::from_be_bytes(
                hasher.finish()[28..32]
                    .try_into()
                    .map_err(|_| eyre::eyre!("SHA256 slice [28..32] was not 4 bytes"))?,
            );
            let mut rng = SimpleRNG::new(rng_seed);

            let last_range_pos_u32 = u32::try_from(self.last_range_pos).map_err(|_| {
                eyre::eyre!("last_range_pos {} exceeds u32::MAX", self.last_range_pos)
            })?;
            let next_range_pos = usize::try_from(rng.next() % last_range_pos_u32)
                .map_err(|_| eyre::eyre!("range position exceeds usize"))?;
            let range = self.ranges[next_range_pos];
            self.ranges[next_range_pos] = self.ranges[self.last_range_pos]; // overwrite returned range with last one
            self.last_range_pos -= 1;
            range
        };

        self.last_recall_ranges.insert(step, range);
        self.last_step_num = step;
        Ok(range)
    }

    pub fn reinitialize(&mut self) {
        info!("Reinitializing ranges");
        self.ranges.clear();
        for i in 0..self.num_recall_ranges_in_partition {
            self.ranges.push(i);
        }
        self.last_range_pos = self.num_recall_ranges_in_partition - 1;

        let last_step_to_keep = self.last_step_num.saturating_sub(NUMBER_OF_KEPT_LAST_STEPS);
        self.last_recall_ranges
            .retain(|k, _| *k > last_step_to_keep);
    }

    pub fn new(num_recall_ranges_in_partition: usize) -> Result<Self> {
        if num_recall_ranges_in_partition == 0 {
            return Err(eyre::eyre!(
                "num_recall_ranges_in_partition must be > 0 (misconfiguration: partition size and/or recall range size)"
            ));
        }
        let mut ranges = Vec::with_capacity(num_recall_ranges_in_partition);
        for i in 0..num_recall_ranges_in_partition {
            ranges.push(i);
        }
        Ok(Self {
            last_range_pos: num_recall_ranges_in_partition - 1,
            ranges,
            num_recall_ranges_in_partition,
            last_step_num: 0,
            last_recall_ranges: HashMap::new(),
        })
    }

    /// Reconstructs recall ranges from given seeds assuming last step number + 1 is the step of the first seed
    pub fn reconstruct(&mut self, next_steps: &H256List, partition_hash: &H256) -> Result<()> {
        let step = self.last_step_num;
        next_steps.0.iter().enumerate().try_for_each(|(i, seed)| {
            let offset =
                u64::try_from(i).map_err(|_| eyre::eyre!("enumerate index {i} exceeds u64"))?;
            self.next_recall_range(step + 1 + offset, seed, partition_hash)?;
            Ok(())
        })
    }

    pub fn reset_step(&mut self, step_num: u64) -> u64 {
        let num_ranges = u64::try_from(self.num_recall_ranges_in_partition)
            .expect("num_recall_ranges_in_partition fits in u64 on all supported platforms");
        reset_step(step_num, num_ranges)
    }
}

/// Validates recall range index for a given step number, seed and partition hash
pub fn recall_range_is_valid(
    recall_range: usize,
    num_recall_ranges_in_partition: usize,
    steps: &H256List,
    partition_hash: &H256,
) -> eyre::Result<()> {
    let reconstructed_range =
        get_recall_range(num_recall_ranges_in_partition, steps, partition_hash)?;
    if reconstructed_range != recall_range {
        Err(eyre::eyre!(
            "Invalid recall range index {}, expected {}",
            recall_range,
            reconstructed_range
        ))
    } else {
        Ok(())
    }
}

/// Construct recall range index from given step seeds and partition hash
pub fn get_recall_range(
    num_recall_ranges_in_partition: usize,
    steps: &H256List,
    partition_hash: &H256,
) -> eyre::Result<usize> {
    let mut ranges = Ranges::new(num_recall_ranges_in_partition)?;
    ranges.reconstruct(steps, partition_hash)?;
    if let Some(reconstructed_range) = ranges.get_last_recall_range() {
        Ok(reconstructed_range)
    } else {
        Err(eyre::eyre!("No recall range index found"))
    }
}

/// Get last step number where ranges were reinitialized
pub fn reset_step_number(step_num: u64, config: &ConsensusConfig) -> u64 {
    let num_recall_ranges_in_partition = num_recall_ranges_in_partition(config);
    reset_step(step_num, num_recall_ranges_in_partition)
}

pub fn reset_step(step_num: u64, num_recall_ranges_in_partition: u64) -> u64 {
    // Prevent arithmetic underflow when step_num is 0
    if step_num == 0 {
        return 0;
    }
    ((step_num - 1) / num_recall_ranges_in_partition) * num_recall_ranges_in_partition + 1
}

pub fn num_recall_ranges_in_partition(config: &ConsensusConfig) -> u64 {
    config
        .num_chunks_in_partition
        .div_ceil(config.num_chunks_in_recall_range)
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_set::HashSet;

    fn create_test_config(chunks: u64, recall_range: u64) -> ConsensusConfig {
        ConsensusConfig {
            num_chunks_in_partition: chunks,
            num_chunks_in_recall_range: recall_range,
            ..ConsensusConfig::testing()
        }
    }

    #[test]
    fn test_efficient_sampling() {
        let num_recall_ranges: usize = 100;
        let partition_hash = H256::random();
        let mut ranges = Ranges::new(100).unwrap();
        let seed = H256::random();

        let mut got_ranges = HashSet::new();

        for i in 1..=num_recall_ranges {
            let step = u64::try_from(i).unwrap();
            let range = ranges
                .get_recall_range(step, &seed, &partition_hash)
                .unwrap();
            assert!(range < num_recall_ranges, "Invalid range idx {range}");
            assert!(!got_ranges.contains(&range), "Repeated range {range}");
            got_ranges.insert(range);

            let range2 = ranges
                .get_recall_range(step, &seed, &partition_hash)
                .unwrap();
            assert_eq!(range, range2, "Cached range should be equal");
        }

        assert_eq!(num_recall_ranges, ranges.last_range_pos + 1);
    }

    #[test]
    fn test_validation() {
        let num_recall_ranges: usize = 10;
        let partition_hash = H256::random();

        let mut seeds = H256List(Vec::new());
        for _ in 0..num_recall_ranges {
            seeds.0.push(H256::random());
        }

        let mut ranges = Ranges::new(num_recall_ranges).unwrap();

        for step in 1..=num_recall_ranges {
            let range = ranges
                .get_recall_range(
                    u64::try_from(step).unwrap(),
                    &seeds[step - 1],
                    &partition_hash,
                )
                .unwrap();
            let res = recall_range_is_valid(
                range,
                num_recall_ranges,
                &H256List(seeds.0[0..step].into()),
                &partition_hash,
            );
            assert!(res.is_ok());
        }
    }

    #[test]
    fn no_underflow_in_reset_step_number() {
        let config = ConsensusConfig::testing();
        let step_num = 0;
        let reset_step_num = reset_step_number(step_num, &config);
        assert_eq!(
            reset_step_num, 0,
            "Reset step number should be 0 for step 0"
        );
    }

    mod num_recall_ranges_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(1000, 100, 10, "Simple exact division")]
        #[case(51_872_000, 800, 64_840, "Production mainnet config")]
        #[case(10, 2, 5, "Default testing config")]
        #[case(1, 1, 1, "Single chunk edge case")]
        #[case(8, 4, 2, "Small exact division")]
        #[case(1600, 800, 2, "Exact boundary for 2 ranges")]
        #[case(1000, 800, 2, "Must round up to 2")]
        #[case(801, 800, 2, "Minimal remainder (1 chunk)")]
        #[case(1599, 800, 2, "Maximum chunks still needing 2 ranges")]
        #[case(1601, 800, 3, "Minimum chunks requiring 3 ranges")]
        #[case(999, 1000, 1, "Partition smaller than recall range")]
        #[case(101, 10, 11, "101 chunks / 10 per range = 11 ranges")]
        #[case(199, 100, 2, "Nearly double with remainder")]
        #[case(1, 1000, 1, "Single chunk, large recall range")]
        #[case(2, 1, 2, "Two ranges of size 1")]
        #[case(100, 1, 100, "Many single-chunk ranges")]
        #[case(3, 2, 2, "Small numbers with remainder")]
        #[case(10_000, 3_333, 4, "Large numbers with remainder")]
        #[case(1_000_000, 333_333, 4, "Very large with remainder")]
        #[case(123_456, 789, 157, "Random large numbers")]
        fn test_num_recall_ranges_ceiling_division(
            #[case] chunks: u64,
            #[case] recall_range: u64,
            #[case] expected: u64,
            #[case] description: &str,
        ) {
            let config = create_test_config(chunks, recall_range);
            let result = num_recall_ranges_in_partition(&config);
            assert_eq!(
                result, expected,
                "{}: {}÷{} should equal {} but got {}",
                description, chunks, recall_range, expected, result
            );
        }

        #[test]
        fn test_zero_chunks() {
            let config = create_test_config(0, 800);
            let result = num_recall_ranges_in_partition(&config);
            assert_eq!(result, 0, "Zero chunks should result in zero ranges");
        }

        #[test]
        fn test_large_number_handling() {
            let config = create_test_config(u64::MAX - 1, u64::MAX);
            let result = num_recall_ranges_in_partition(&config);
            assert_eq!(result, 1, "Should handle near-maximum values");

            let config2 = create_test_config(1_000_000_000, 3);
            let result2 = num_recall_ranges_in_partition(&config2);
            let expected2 = 1_000_000_000_u64.div_ceil(3);
            assert_eq!(result2, expected2, "Should handle large dividend correctly");
        }

        #[test]
        fn test_integration_with_ranges_struct() {
            let configs_with_remainders = [(1000, 800), (801, 800), (1601, 800)];

            for (chunks, recall) in configs_with_remainders {
                let config = create_test_config(chunks, recall);
                let num_ranges = num_recall_ranges_in_partition(&config);

                let nr = usize::try_from(num_ranges).unwrap();
                let ranges = Ranges::new(nr).unwrap();
                assert_eq!(
                    ranges.num_recall_ranges_in_partition, nr,
                    "Ranges struct should initialize with ceiling division result"
                );

                assert_eq!(ranges.ranges.len(), nr);
                assert_eq!(ranges.last_range_pos, nr - 1);
            }
        }

        #[rstest]
        #[case(1, 11, 1, "Step 1 with 11 ranges")]
        #[case(11, 11, 1, "Step 11 with 11 ranges")]
        #[case(12, 11, 12, "Step 12 with 11 ranges (new cycle)")]
        #[case(22, 11, 12, "Step 22 with 11 ranges")]
        #[case(23, 11, 23, "Step 23 with 11 ranges (third cycle)")]
        #[case(100, 11, 100, "Step 100 with 11 ranges")]
        #[case(110, 11, 100, "Step 110 with 11 ranges")]
        #[case(111, 11, 111, "Step 111 with 11 ranges")]
        fn test_reset_step_with_ceiling_ranges(
            #[case] step: u64,
            #[case] num_ranges: u64,
            #[case] expected_reset: u64,
            #[case] description: &str,
        ) {
            let result = reset_step(step, num_ranges);
            assert_eq!(
                result, expected_reset,
                "{}: reset_step({}, {}) should be {}",
                description, step, num_ranges, expected_reset
            );
        }

        #[test]
        fn test_validation_with_partial_last_range() {
            let config = create_test_config(101, 10);
            let num_ranges = usize::try_from(num_recall_ranges_in_partition(&config)).unwrap();
            let partition_hash = H256::random();

            let mut seeds = H256List(Vec::new());
            for _ in 0..num_ranges {
                seeds.0.push(H256::random());
            }

            let mut ranges = Ranges::new(num_ranges).unwrap();

            for step in 1..=num_ranges {
                let range = ranges
                    .get_recall_range(
                        u64::try_from(step).unwrap(),
                        &seeds[step - 1],
                        &partition_hash,
                    )
                    .unwrap();
                let res = recall_range_is_valid(
                    range,
                    num_ranges,
                    &H256List(seeds.0[0..step].into()),
                    &partition_hash,
                );
                assert!(
                    res.is_ok(),
                    "Validation should pass for step {} with partial last range",
                    step
                );
            }
        }
    }

    mod proptest_properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn reset_step_idempotent(
                step in 1_u64..=100_000,
                num_ranges in 1_u64..=1000,
            ) {
                let reset = reset_step(step, num_ranges);
                let double_reset = reset_step(reset, num_ranges);
                prop_assert_eq!(reset, double_reset);
            }

            #[test]
            fn reset_step_within_bounds(
                step in 1_u64..=100_000,
                num_ranges in 1_u64..=1000,
            ) {
                let reset = reset_step(step, num_ranges);
                prop_assert!(reset >= 1);
                prop_assert!(reset <= step);
            }

            #[test]
            fn reset_step_cycle_boundary(
                cycle in 0_u64..=100,
                num_ranges in 1_u64..=1000,
            ) {
                let first_step = cycle * num_ranges + 1;
                let reset = reset_step(first_step, num_ranges);
                prop_assert_eq!(reset, first_step);
            }

            #[test]
            fn sampling_uniqueness(
                num_ranges in 1_usize..=50,
                seed_bytes in proptest::array::uniform32(0_u8..),
                hash_bytes in proptest::array::uniform32(0_u8..),
            ) {
                let mut ranges = Ranges::new(num_ranges).unwrap();
                let seed = H256(seed_bytes);
                let partition_hash = H256(hash_bytes);
                let mut seen = HashSet::new();
                for step in 1..=num_ranges {
                    let r = ranges.get_recall_range(u64::try_from(step).unwrap(), &seed, &partition_hash).unwrap();
                    prop_assert!(seen.insert(r), "duplicate range index {} at step {}", r, step);
                }
            }

            #[test]
            fn sampling_determinism(
                num_ranges in 1_usize..=30,
                seed_bytes in proptest::array::uniform32(0_u8..),
                hash_bytes in proptest::array::uniform32(0_u8..),
            ) {
                let seed = H256(seed_bytes);
                let partition_hash = H256(hash_bytes);

                let mut ranges_a = Ranges::new(num_ranges).unwrap();
                let mut result_a = Vec::with_capacity(num_ranges);
                for step in 1..=num_ranges {
                    result_a.push(ranges_a.get_recall_range(u64::try_from(step).unwrap(), &seed, &partition_hash).unwrap());
                }

                let mut ranges_b = Ranges::new(num_ranges).unwrap();
                let mut result_b = Vec::with_capacity(num_ranges);
                for step in 1..=num_ranges {
                    result_b.push(ranges_b.get_recall_range(u64::try_from(step).unwrap(), &seed, &partition_hash).unwrap());
                }

                prop_assert_eq!(result_a, result_b);
            }
        }
    }

    mod edge_case_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(0, 5, "zero ranges requested")]
        #[case(1, 0, "zero available ranges")]
        #[case(100, 1, "only 1 range available")]
        fn k_gt_n_handling(#[case] k: usize, #[case] n: usize, #[case] description: &str) {
            if n == 0 {
                // Ranges::new(0) must return Err — zero partition ranges is a misconfiguration.
                let result = Ranges::new(0);
                assert!(
                    result.is_err(),
                    "{}: Ranges::new(0) should be Err",
                    description
                );
                return;
            }
            let seed = H256::random();
            let partition_hash = H256::random();
            let mut ranges = Ranges::new(n).unwrap();
            let samples: Vec<usize> = (1..=k)
                .map(|step| {
                    ranges
                        .get_recall_range(u64::try_from(step).unwrap(), &seed, &partition_hash)
                        .unwrap()
                })
                .collect();
            assert_eq!(
                samples.len(),
                k,
                "{}: expected {} samples, got {}",
                description,
                k,
                samples.len()
            );
        }

        #[rstest]
        #[case(0, 10, "recall_range out of bounds (0, 10 ranges)")]
        #[case(10, 10, "recall_range equals num_ranges (boundary)")]
        #[case(99, 5, "recall_range far exceeds num_ranges")]
        fn recall_range_is_valid_err_path(
            #[case] bad_range: usize,
            #[case] num_ranges: usize,
            #[case] description: &str,
        ) {
            let partition_hash = H256::random();
            let mut seeds = H256List(Vec::new());
            seeds.0.push(H256::random());

            let valid_range = get_recall_range(num_ranges, &seeds, &partition_hash).unwrap();
            let invalid_range = if bad_range == valid_range {
                num_ranges
            } else {
                bad_range
            };
            let result = recall_range_is_valid(invalid_range, num_ranges, &seeds, &partition_hash);
            assert!(
                result.is_err(),
                "{}: expected Err for range {} (valid was {})",
                description,
                invalid_range,
                valid_range
            );
        }
    }
}
