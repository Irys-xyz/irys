use crate::{H256, NUM_RECALL_RANGES_IN_PARTITION};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

/// Efficient sampling: randomly picks partition ranges idxs in [0..NUM_RECALL_RANGES_IN_PARTITION-1] interval without repetiting upto automatic reinitialization after all idxs are retrived.
pub struct Ranges {
    /// Available ranges idxs
    ranges: [u32; NUM_RECALL_RANGES_IN_PARTITION as usize], // test checks cast is safe
    /// last valid range position
    last_range_pos: usize,
}

impl Ranges {
    /// Picks next random (using seed as entropy) range idx in [0..NUM_RECALL_RANGES_IN_PARTITION-1] interval
    pub fn next_recall_range(&mut self, seed: H256) -> u32 {
        if self.last_range_pos == 0 {
            let range = self.ranges[0];
            self.reinitialize();
            return range;
        }
        let mut rng = ChaCha20Rng::from_seed(seed.into());
        let next_range_pos = (rng.next_u32() % self.last_range_pos as u32) as usize; // usize (one word in current CPU acrchitecutre) to u32 is safe in 32bits of above architectures
        let range = self.ranges[next_range_pos];
        self.ranges[next_range_pos] = self.ranges[self.last_range_pos]; // overwrite returned range with last one
        self.last_range_pos -= 1;
        range
    }

    fn ranges_number(self) -> usize {
        self.last_range_pos + 1
    }

    fn reinitialize(&mut self) {
        *self = Self::new()
    }

    pub fn new() -> Self {
        let mut ranges: [u32; NUM_RECALL_RANGES_IN_PARTITION as usize] =
            [0; NUM_RECALL_RANGES_IN_PARTITION as usize];
        for i in 0..NUM_RECALL_RANGES_IN_PARTITION as usize {
            ranges[i] = i as u32
        }
        Self {
            last_range_pos: (NUM_RECALL_RANGES_IN_PARTITION - 1) as usize,
            ranges,
        }
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use arbitrary::Arbitrary;
    use std::collections::hash_set::HashSet;

    #[test]
    fn test_cast_safe() {
        // check cast is safe!
        let ranges_size: usize = NUM_RECALL_RANGES_IN_PARTITION.try_into().expect(
            "NUM_RECALL_RANGES_IN_PARTITION representation overflows when casted as usize!",
        );
        assert!(ranges_size > 0)
    }

    #[test]
    fn test_it_works() {
        let mut ranges = Ranges::new();
        let bytes: [u8; 16] = [
            255, 40, 179, 24, 184, 113, 24, 73, 143, 51, 152, 121, 133, 143, 14, 59,
        ];
        let mut u = arbitrary::Unstructured::new(&bytes);
        let seed = H256::arbitrary(&mut u).unwrap();

        let mut got_ranges = HashSet::new();

        // no repeated range
        for _i in 0..NUM_RECALL_RANGES_IN_PARTITION {
            let range = ranges.next_recall_range(seed);
            assert!(
                (range as u64) < NUM_RECALL_RANGES_IN_PARTITION,
                "Invalid range idx {range}"
            );
            assert!(!got_ranges.contains(&range), "Repeated range {range}");
            got_ranges.insert(range);
        }

        // ranges are reinitialized after all posible ranges are retrived
        assert_eq!(
            NUM_RECALL_RANGES_IN_PARTITION as usize,
            ranges.ranges_number()
        )
    }
}
