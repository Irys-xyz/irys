use actix::prelude::*;
use irys_actors::block_index_service::BlockIndexReadGuard;
use irys_database::block_header_by_hash;
use irys_types::{block_production::Seed, Config, DatabaseProvider, H256List, H256};
use nodit::{interval::ii, InclusiveInterval, Interval};
use reth_db::Database;
use reth_db_api::database::Database;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
    time::Duration,
};
use tokio::time::sleep;
use tracing::{info, warn};

impl VdfState {
    /// Creates a new `VdfService` setting up how many steps are stored in memory, and loads state from path if available
    pub fn new(
        block_index: BlockIndexReadGuard,
        db: DatabaseProvider,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
        config: &Config,
    ) -> Self {
        create_state(block_index, db, vdf_mining_state_sender, &config)
    }

    pub fn from_capacity(capacity: usize) -> Self {
        Self {
            global_step: 0,
            capacity,
            seeds: VecDeque::with_capacity(capacity),
            mining_state_sender: None,
        }
    }
    pub fn get_last_step_and_seed(&self) -> (u64, Option<Seed>) {
        (self.global_step, self.seeds.back().cloned())
    }

    /// Called when local vdf thread generates a new step, or vdf step synced from another peer, and we want to increment vdf step state
    pub fn increment_step(&mut self, seed: Seed) {
        if self.seeds.len() >= self.capacity {
            self.seeds.pop_front();
        }
        self.global_step += 1;
        self.seeds.push_back(seed);
        info!(
            "Received seed: {:?} global step: {}",
            self.seeds.back().unwrap(),
            self.global_step
        );
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        let vdf_steps_len = self.seeds.len() as u64;

        let last_global_step = self.global_step;

        // first available global step should be at least one.
        // TODO: Should this instead panic! as something has gone very wrong?
        let first_global_step = last_global_step.saturating_sub(vdf_steps_len) + 1;

        if first_global_step > last_global_step {
            return Err(eyre::eyre!("No steps stored!"));
        }

        if !ii(first_global_step, last_global_step).contains_interval(&i) {
            return Err(eyre::eyre!(
                "Unavailable requested range ({}..={}). Stored steps range is ({}..={})",
                i.start(),
                i.end(),
                first_global_step,
                last_global_step
            ));
        }

        let start: usize = (i.start() - first_global_step).try_into()?;
        let end: usize = (i.end() - first_global_step).try_into()?;

        Ok(H256List(
            self.seeds
                .range(start..=end)
                .map(|seed| seed.0)
                .collect::<Vec<H256>>(),
        ))
    }
}

/// create VDF state using the latest block in db
fn create_state(
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
    vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);

    if let Some(block_hash) = block_index
        .read()
        .get_latest_item()
        .map(|item| item.block_hash)
    {
        let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
        let tx = db.tx().unwrap();

        let mut block = block_header_by_hash(&tx, &block_hash, false)
            .unwrap()
            .unwrap();
        let global_step_number = block.vdf_limiter_info.global_step_number;
        let mut steps_remaining = capacity;

        while steps_remaining > 0 && block.height > 0 {
            // get all the steps out of the block
            for step in block.vdf_limiter_info.steps.0.iter().rev() {
                seeds.push_front(Seed(*step));
                steps_remaining -= 1;
                if steps_remaining == 0 {
                    break;
                }
            }
            // get the previous block
            block = block_header_by_hash(&tx, &block.previous_block_hash, false)
                .unwrap()
                .unwrap();
        }
        info!(
            "Initializing vdf service from block's info in step number {}",
            global_step_number
        );
        return VdfState {
            global_step: global_step_number,
            seeds,
            capacity,
            mining_state_sender: Some(vdf_mining_state_sender),
        };
    };

    info!("No block index found, initializing VdfState from zero");
    VdfState {
        global_step: 0,
        seeds: VecDeque::with_capacity(capacity),
        capacity,
        mining_state_sender: Some(vdf_mining_state_sender),
    }
}

/// return the larger of MINIMUM_CAPACITY or number of seeds required for (chunks in partition / chunks in recall range)
/// This ensure the capacity of VecDeqeue is large enough for the partition.
pub fn calc_capacity(config: &Config) -> usize {
    const MINIMUM_CAPACITY: u64 = 10_000;
    let capacity_from_config: u64 =
        config.consensus.num_chunks_in_partition / config.consensus.num_chunks_in_recall_range;
    let capacity = if capacity_from_config < MINIMUM_CAPACITY {
        warn!(
            "capacity in config: {} set too low. Overridden with {}",
            capacity_from_config, MINIMUM_CAPACITY
        );
        MINIMUM_CAPACITY
    } else {
        std::cmp::max(MINIMUM_CAPACITY, capacity_from_config)
    };

    capacity.try_into().expect("expected u64 to cast to u32")
}
