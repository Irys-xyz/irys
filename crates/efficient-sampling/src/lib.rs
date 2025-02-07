use std::collections::HashMap;

use irys_types::{H256List, SimpleRNG, StorageConfig, H256};
use openssl::sha;
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
    pub fn get_recall_range(&mut self, step: u64, seed: &H256, partition_hash: &H256) -> usize {
        if self.last_recall_ranges.contains_key(&step) {
            let range = *self.last_recall_ranges.get(&step).unwrap();
            debug!(
                "Partition hash {}, Recall range for step {} is cached, range {}/{}",
                partition_hash, step, range, self.num_recall_ranges_in_partition
            );
            range
        } else {
            let range = self.next_recall_range(step, seed, partition_hash);
            debug!("Partition hash {}, Recall range for step {} is not cached, calling next range, range {}/{}", partition_hash, step, range, self.num_recall_ranges_in_partition);
            range
        }
    }

    pub fn get_last_recall_range(self) -> Option<usize> {
        self.last_recall_ranges.get(&self.last_step_num).copied()
    }

    /// Picks next random (using seed as entropy) range idx in [0..NUM_RECALL_RANGES_IN_PARTITION-1] interval
    pub fn next_recall_range(&mut self, step: u64, seed: &H256, partition_hash: &H256) -> usize {
        // non consecutive vdf_steps is handled at mining level
        if step != self.last_step_num + 1 {
            panic!("Non consecutive vdf steps are not supported, last step num {}, current step num {}", self.last_step_num, step);
        }

        let range = if self.last_range_pos == 0 {
            let range = self.ranges[0];
            self.reinitialize();
            range
        } else {
            let mut hasher = sha::Sha256::new();
            hasher.update(&seed.0);
            hasher.update(&partition_hash.0);
            let rng_seed: u32 = u32::from_be_bytes(hasher.finish()[28..32].try_into().unwrap());
            let mut rng = SimpleRNG::new(rng_seed);

            let next_range_pos = (rng.next() % self.last_range_pos as u32) as usize; // usize (one word in current CPU architecture) to u32 is safe in 32bits of above architectures
            let range = self.ranges[next_range_pos];
            self.ranges[next_range_pos] = self.ranges[self.last_range_pos]; // overwrite returned range with last one
            self.last_range_pos -= 1;
            range
        };

        self.last_recall_ranges.insert(step, range);
        self.last_step_num = step;
        range
    }

    pub fn reinitialize(&mut self) {
        info!("Reinitializing ranges");
        self.ranges.clear();
        for i in 0..self.num_recall_ranges_in_partition {
            self.ranges.push(i);
        }
        self.last_range_pos = self.num_recall_ranges_in_partition - 1;

        let last_step_to_keep = if self.last_step_num >= NUMBER_OF_KEPT_LAST_STEPS {
            self.last_step_num - NUMBER_OF_KEPT_LAST_STEPS
        } else {
            0
        };
        self.last_recall_ranges
            .retain(|k, _| *k > last_step_to_keep);
    }

    pub fn new(num_recall_ranges_in_partition: usize) -> Self {
        let mut ranges = Vec::with_capacity(num_recall_ranges_in_partition);
        for i in 0..num_recall_ranges_in_partition {
            ranges.push(i);
        }
        Self {
            last_range_pos: num_recall_ranges_in_partition - 1,
            ranges,
            num_recall_ranges_in_partition,
            last_step_num: 0,
            last_recall_ranges: HashMap::new(),
        }
    }

    /// Reconstructs recall ranges from given seeds assuming last step number + 1 is the step of the first seed
    pub fn reconstruct(&mut self, next_steps: &H256List, partition_hash: &H256) {
        let step = self.last_step_num;
        next_steps.0.iter().enumerate().for_each(|(i, seed)| {
            self.next_recall_range(step + 1 + i as u64, seed, partition_hash);
        });
    }

    pub fn reset_step(&mut self, step_num: u64) -> u64 {
        reset_step(step_num, self.num_recall_ranges_in_partition as u64)
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
    let mut ranges = Ranges::new(num_recall_ranges_in_partition);
    ranges.reconstruct(steps, partition_hash);
    if let Some(reconstructed_range) = ranges.get_last_recall_range() {
        Ok(reconstructed_range)
    } else {
        Err(eyre::eyre!("No recall range index found"))
    }
}

/// Get last step number where ranges were reinitialized
pub fn reset_step_number(step_num: u64, config: &StorageConfig) -> u64 {
    let num_recall_ranges_in_partition = num_recall_ranges_in_partition(config);
    reset_step(step_num, num_recall_ranges_in_partition)
}

pub fn reset_step(step_num: u64, num_recall_ranges_in_partition: u64) -> u64 {
    ((step_num - 1) / num_recall_ranges_in_partition) * num_recall_ranges_in_partition + 1
}

pub fn num_recall_ranges_in_partition(config: &StorageConfig) -> u64 {
    config.num_chunks_in_partition / config.num_chunks_in_recall_range
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_set::HashSet;

    #[test]
    fn test_efficient_sampling() {
        let num_recall_ranges = 100;
        let partition_hash = H256::random();
        let mut ranges = Ranges::new(100);
        let seed = H256::random();

        let mut got_ranges = HashSet::new();

        // check for no repeated range index
        for i in 1..=num_recall_ranges {
            let range = ranges.get_recall_range(i, &seed, &partition_hash);
            assert!(
                (range as u64) < num_recall_ranges,
                "Invalid range idx {range}"
            );
            assert!(!got_ranges.contains(&range), "Repeated range {range}");
            got_ranges.insert(range);

            // get the same cached range
            let range2 = ranges.get_recall_range(i, &seed, &partition_hash);
            assert_eq!(range, range2, "Cached range should be equal");
        }

        // check ranges are reinitialized after all possible ranges are retrieved
        assert_eq!(num_recall_ranges as usize, ranges.last_range_pos + 1,)
    }

    #[test]
    fn test_validation() {
        let num_recall_ranges: usize = 10;
        let partition_hash = H256::random();

        let mut seeds = H256List(Vec::new());
        for _ in 0..num_recall_ranges {
            seeds.0.push(H256::random());
        }

        let mut ranges = Ranges::new(num_recall_ranges);

        for step in 1..=num_recall_ranges {
            let range = ranges.get_recall_range(step as u64, &seeds[step - 1], &partition_hash);
            let res = recall_range_is_valid(
                range,
                num_recall_ranges,
                &H256List(seeds.0[0..step].into()),
                &partition_hash,
            );
            assert!(res.is_ok());
        }
    }
}
