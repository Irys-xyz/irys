use actix::prelude::*;
use nodit::{interval::ii, InclusiveInterval, Interval};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
    time::Duration,
};

use tracing::{info, warn};

use irys_types::{block_production::Seed, H256List, H256};

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

use tokio::time::sleep;

#[derive(Debug, Clone, Default)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
}

impl VdfState {
    pub fn get_last_step_and_seed(&self) -> (u64, Option<Seed>) {
        (self.global_step, self.seeds.back().cloned())
    }

    /// Called when local vdf thread generates a new step and wants to increment vdf step state
    pub fn push_step(&mut self, seed: Seed) {
        self.update_state(seed, self.global_step + 1);
    }

    /// Called when vdf step is recieved during peer sync and we need to jump to specified step
    pub fn jump_step(&mut self, seed: Seed, step: u64) {
        self.update_state(seed, step);
    }

    // FIXME: update_state() might not be ok to jump... it might need to fill in the gap too. How to know?
    //        would that create the recall_range error we see when discovering a block?

    /// Push new seed, removing oldest one if capacity full
    fn update_state(&mut self, seed: Seed, step: u64) {
        if self.seeds.len() >= self.capacity {
            self.seeds.pop_front();
        }
        self.global_step = step;
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
        let first_global_step = last_global_step - vdf_steps_len + 1;

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

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct VdfStepsReadGuard(AtomicVdfState);

impl VdfStepsReadGuard {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(state: Arc<RwLock<VdfState>>) -> Self {
        Self(state)
    }

    pub fn into_inner_cloned(&self) -> AtomicVdfState {
        self.0.clone()
    }

    /// Read access to internal steps queue
    pub fn read(&self) -> RwLockReadGuard<'_, VdfState> {
        self.0.read().unwrap()
    }

    /// Try to read steps interval pooling a max. of 10 times waiting for interval to be available
    /// TODO @ernius: remove this method usage after VDF validation is done async, vdf steps validation reads VDF steps blocking last steps pushes so the need of this pooling.
    pub async fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        const MAX_RETRIES: i32 = 10;
        for attempt in 0..MAX_RETRIES {
            match self.read().get_steps(i) {
                        Ok(c) => return Ok(c),
                        Err(e) =>
                            warn!("Requested vdf steps range {:?} still unavailable, attempt: {}, reason: {:?}, waiting ...", &i, attempt, e),
                    };
            // should be similar to a yield
            sleep(Duration::from_millis(200)).await;
        }
        Err(eyre::eyre!(
            "Max. retries reached while waiting to get VDF steps!"
        ))
    }
}
