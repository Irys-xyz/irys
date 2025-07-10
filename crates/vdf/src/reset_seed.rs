use crate::apply_reset_seed;
use irys_types::block_provider::BlockProvider;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader, H256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::info;

/// A struct that notifies VDF when a new block is created or received over the gossip.
/// It is necessary to correctly apply the VDF reset seed to the next step.
#[derive(Clone, Debug)]
pub struct ResetSeed {
    /// Step number of the reset step.
    pub global_step_number: u64,
    /// Parent block hash of the block that contains the reset step.
    pub seed: H256,
    pub block_hash: BlockHash,
    pub block_height: u64,
}

impl ResetSeed {
    /// Extracts a `ResetSeed` from the block header if it contains a reset step. Returns `None` if
    /// the block header does not contain a reset step.
    pub fn extract_block_header(
        block_header: &IrysBlockHeader,
        reset_frequency: u64,
    ) -> Option<Self> {
        block_header
            .vdf_limiter_info
            .contains_reset_step(reset_frequency)
            .map(|step_number| ResetSeed {
                global_step_number: step_number,
                seed: block_header.previous_block_hash,
                block_hash: block_header.block_hash,
                block_height: block_header.height,
            })
    }
}

/// During the block production process, we need to determine whether the steps in the block header
/// content a step with a number/config.reset_frequency == 0. If it does, then we set the new_seed
/// field of this block header to the block hash of the previous block, then proceed as usual. During
/// the block validation (probably prevalidation?) we need to check that the header contains a step with
/// a number/config.reset_frequency == 0, and that the new_seed field is equal to the block hash of the
/// previous block. If it is not, then we return an error. Once the block that has a
/// reset step is being moved out of the tree, we send a NewResetSeed message to the VDF thread. When
/// the next reset step is reached, the VDF thread will use the block hash from the NewResetSeed message
/// to apply_reset to the VDF state.
pub struct ResetSeedManager<B: BlockProvider> {
    current_reset_seed: H256,
    possible_reset_seeds: HashMap<u64, HashMap<BlockHash, ResetSeed>>,
    reset_frequency: u64,
    block_provider: B,
}

impl<B: BlockProvider> ResetSeedManager<B> {
    pub fn new(initial_reset_seed: H256, reset_frequency: u64, block_status_provider: B) -> Self {
        ResetSeedManager {
            current_reset_seed: initial_reset_seed,
            possible_reset_seeds: HashMap::new(),
            reset_frequency,
            block_provider: block_status_provider,
        }
    }
    pub fn add_reset_seed_candidate(&mut self, reset_seed: ResetSeed) {
        let global_step_number = reset_seed.global_step_number;
        let block_hash = reset_seed.block_hash;

        // Insert the reset seed into the map, creating a new entry if necessary
        self.possible_reset_seeds
            .entry(global_step_number)
            .or_default()
            .insert(block_hash, reset_seed);
    }

    pub fn reset_seed_by_block_hash(
        &self,
        global_step_number: u64,
        block_hash: BlockHash,
    ) -> Option<&ResetSeed> {
        self.possible_reset_seeds
            .get(&global_step_number)
            .and_then(|map| map.get(&block_hash))
    }

    pub fn remove_reset_seed(
        &mut self,
        global_step_number: u64,
        block_hash: BlockHash,
    ) -> Option<ResetSeed> {
        self.possible_reset_seeds
            .get_mut(&global_step_number)
            .and_then(|map| map.remove(&block_hash))
    }

    pub fn find_new_seed(&self, step_number: u64) -> Option<&ResetSeed> {
        let candidate_hashes = self.possible_reset_seeds.get(&step_number)?;
        let possible_finalized_seeds = candidate_hashes
            .values()
            .filter(|reset_seed| {
                self.block_provider
                    .does_block_exist(&reset_seed.block_hash, reset_seed.block_height)
            })
            .collect::<Vec<_>>();

        // If no finalized reset seeds are found, return None
        if possible_finalized_seeds.is_empty() {
            return None;
        }
        // It shouldn't be possible to have more than one finalized reset seed for a given step number
        if possible_finalized_seeds.len() > 1 {
            panic!(
                "Multiple reset seeds found for step {}: {:?}",
                step_number, possible_finalized_seeds
            );
        }
        // Return the only finalized reset seed
        Some(possible_finalized_seeds[0])
    }

    pub fn remove_all_candidates_for_step(
        &mut self,
        step_number: u64,
    ) -> HashMap<BlockHash, ResetSeed> {
        self.possible_reset_seeds
            .remove(&step_number)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn process_reset(&mut self, global_step_number: u64, hash: H256) -> H256 {
        if global_step_number % self.reset_frequency == 0 {
            info!("Processing reset for step {}", global_step_number);
            let seed = if let Some(reset_seed) = self.find_new_seed(global_step_number).cloned() {
                self.current_reset_seed = reset_seed.seed;
                self.remove_all_candidates_for_step(global_step_number);
                info!(
                    "Found new reset seed for {}: {:?}",
                    global_step_number, reset_seed.seed
                );
                reset_seed.seed
            } else {
                info!(
                    "No new reset seed found for step {}, using existing one",
                    global_step_number
                );
                self.current_reset_seed.clone()
            };
            info!(
                "Reset seed {:?} applied to step {}",
                global_step_number, seed
            );
            apply_reset_seed(hash, seed)
        } else {
            hash
        }
    }
}
