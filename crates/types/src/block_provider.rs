use crate::{BlockHash, DatabaseProvider, IrysBlockHeader, VDFLimiterInfo, H256};

/// A trait that is used to provide access to blocks by their hash. Used to avoid circular dependencies,
/// such as between VDF and BlockIndexService.
pub trait BlockProvider {
    fn latest_canonical_vdf_info(&self) -> Option<VDFLimiterInfo>;
}

pub trait BlockIndex {
    /// Checks if the given block height and hash are canonical.
    fn contains_block(&self, height: u64, hash: BlockHash) -> bool;
}

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::io::AsyncReadExt;

#[derive(Debug)]
struct ResetSeedManagerInner<BI: BlockIndex> {
    possible_reset_seed_heights: HashMap<u64, Vec<(u64, H256)>>,
    block_index: BI,
}

impl<BI: BlockIndex> ResetSeedManagerInner<BI> {
    fn new(block_index: BI) -> Self {
        // let block_step = latest_block.vdf_limiter_info.global_step_number;
        // let next_seed = latest_block.vdf_limiter_info.next_seed;
        // let previous_reset_step = block_step - (block_step % reset_frequency);
        // let reset_block_height = reset_block.height;

        let mut possible_reset_seed_heights = HashMap::new();

        Self {
            possible_reset_seed_heights,
            block_index,
        }
    }

    fn block_hash_that_contains_step(&self, step: u64) -> Option<H256> {
        let possible_reset_heights = self.possible_reset_seed_heights.get(&step)?;
        possible_reset_heights
            .iter()
            .find_map(|(block_height, block_hash)| {
                if self.block_index.contains_block(*block_height, *block_hash) {
                    Some(*block_hash)
                } else {
                    None
                }
            })
    }

    fn remove_heights_for_step(&mut self, step: u64) {
        self.possible_reset_seed_heights.remove(&step);
    }

    fn add_height_that_contains_step(&mut self, step: u64, block_height: u64, block_hash: H256) {
        self.possible_reset_seed_heights
            .entry(step)
            .or_default()
            .push((block_height, block_hash));
    }
}

#[derive(Clone, Debug)]
pub struct ResetSeedCache<BI: BlockIndex> {
    inner: Arc<RwLock<ResetSeedManagerInner<BI>>>,
}

impl<BI: BlockIndex> ResetSeedCache<BI> {
    pub fn new(block_index: BI) -> Self {
        Self {
            inner: Arc::new(RwLock::new(ResetSeedManagerInner::new(block_index))),
        }
    }

    /// Searches for a block hash that contains the given step in the BlockIndex.
    pub fn block_hash_that_contains_step(&self, step: u64) -> Option<H256> {
        let inner = self
            .inner
            .read()
            .expect("Failed to lock ResetSeedManagerInner for writing");
        inner.block_hash_that_contains_step(step)
    }

    /// Removes all heights that are associated with the given step.
    pub fn remove_heights_for_step(&self, step: u64) {
        let mut inner = self
            .inner
            .write()
            .expect("Failed to lock ResetSeedManagerInner for writing");
        inner.remove_heights_for_step(step);
    }

    /// Record a block that contains a specific step. They're filtered later when the block
    /// is finalized.
    pub fn record_block_that_contains_step(&self, step: u64, block_height: u64, block_hash: H256) {
        let mut inner = self
            .inner
            .write()
            .expect("Failed to lock ResetSeedManagerInner for writing");
        inner.add_height_that_contains_step(step, block_height, block_hash);
    }

    pub fn remove_old_steps(&self, current_step: u64, reset_frequency: u64) {
        if current_step < reset_frequency * 4 {
            return;
        }
        let cutoff_step = current_step - reset_frequency * 4;
        let mut inner = self
            .inner
            .write()
            .expect("Failed to lock ResetSeedManagerInner for writing");
        inner
            .possible_reset_seed_heights
            .retain(|&k, _| k > cutoff_step);
    }
}
