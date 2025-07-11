use crate::{apply_reset_seed, step_number_to_salt_number, vdf_sha, warn_mismatches};
use eyre::eyre;
use irys_database::{block_header_by_hash, BlockIndex};
use irys_efficient_sampling::num_recall_ranges_in_partition;
use irys_types::{
    block_production::Seed, Config, DatabaseProvider, H256List, VDFLimiterInfo, VdfConfig, H256,
    U256,
};
use nodit::{interval::ii, InclusiveInterval as _, Interval};
use rayon::prelude::*;
use reth_db::Database as _;
use sha2::{Digest as _, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
};
use tokio::{
    sync::mpsc::Sender,
    time::{sleep, Duration},
};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Default)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
    /// whether the VDF thread is mining or paused
    pub mining_state_sender: Option<Sender<bool>>,
    /// global step from the latest canonical block
    global_step_from_the_latest_canonical_block: u64,
    /// minimum global step to keep in the seeds VecDeque
    minimum_step_to_keep: u64,
}

impl VdfState {
    pub fn new(
        capacity: usize,
        global_step: u64,
        mining_state_sender: Option<Sender<bool>>,
    ) -> Self {
        Self {
            global_step,
            global_step_from_the_latest_canonical_block: global_step,
            minimum_step_to_keep: global_step.saturating_sub(capacity as u64),
            seeds: VecDeque::with_capacity(capacity),
            capacity,
            mining_state_sender,
        }
    }

    pub fn set_canonical_step(&mut self, global_canonical_step: u64) {
        self.global_step_from_the_latest_canonical_block = global_canonical_step;
        self.minimum_step_to_keep = global_canonical_step.saturating_sub(self.capacity as u64);
    }

    pub fn canonical_step(&self) -> u64 {
        self.global_step_from_the_latest_canonical_block
    }

    pub fn get_last_step_and_seed(&self) -> (u64, Seed) {
        (
            self.global_step,
            self.seeds
                .back()
                .cloned()
                .expect("To have at least the genesis step to be inserted"),
        )
    }

    pub fn store_step(&mut self, seed: Seed, global_step: u64) -> u64 {
        if self.global_step >= global_step {
            return self.global_step;
        }
        let vdf_depth = global_step.saturating_sub(self.minimum_step_to_keep) as usize;
        if self.seeds.len() >= vdf_depth {
            self.seeds.pop_front();
        }
        if self.global_step + 1 == global_step {
            self.seeds.push_back(seed);
            self.global_step += 1;
        } else {
            panic!("VDF steps can't have gaps and have to be inserted in sequence");
        }
        global_step
    }

    /// Called when local vdf thread generates a new step, or vdf step synced from another peer, and we want to increment vdf step state
    pub fn increment_step(&mut self, seed: Seed) -> u64 {
        let new_step = self.global_step + 1;
        self.store_step(seed, new_step);
        new_step
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

    pub async fn start_mining(&self) -> eyre::Result<()> {
        self.mining_state_sender
            .as_ref()
            .ok_or(eyre!("Mining state sender isn't set!"))?
            .send(true)
            .await
            .map_err(|err| eyre!("failed to send false to mining_state_sender: {:?}", err))
    }

    pub async fn stop_mining(&self) -> eyre::Result<()> {
        self.mining_state_sender
            .as_ref()
            .ok_or(eyre!("Mining state sender isn't set!"))?
            .send(false)
            .await
            .map_err(|err| eyre!("failed to send false to mining_state_sender: {:?}", err))
    }
}

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct VdfStateReadonly(AtomicVdfState);

impl VdfStateReadonly {
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

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        self.read().get_steps(i)
    }

    /// Get a specific step by step number
    pub fn get_step(&self, step_number: u64) -> eyre::Result<H256> {
        self.get_steps(ii(step_number, step_number))?
            .0
            .first()
            .copied()
            .ok_or(eyre!("Step not found"))
    }

    /// Wait for a specific step to be available for n seconds. This doesn't have the timeout.
    /// Instead, we should check that the `desired_step_number` is a reasonable number of steps
    /// to wait for. This should be ensured before calling this function
    pub async fn wait_for_step(&self, desired_step_number: u64) {
        debug!("Waiting for step {}", desired_step_number);
        let retries_per_second = 20;
        loop {
            if self.read().global_step >= desired_step_number {
                debug!("Step {} is available", desired_step_number);
                return;
            }
            sleep(Duration::from_millis(1000 / retries_per_second)).await;
        }
    }
}

/// create VDF state using the latest block in db
pub fn create_state(
    block_index: Arc<RwLock<BlockIndex>>,
    db: DatabaseProvider,
    vdf_mining_state_sender: Sender<bool>,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);

    let block_hash = block_index
        .read()
        .expect("To unlock block index")
        .get_latest_item()
        .map(|item| item.block_hash)
        .expect("To have at least genesis block");

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

    if block.height == 0 {
        seeds.push_front(Seed(block.vdf_limiter_info.steps[0]));
    }

    info!(
        "Initializing vdf service from block's info in step number {}",
        global_step_number
    );

    VdfState {
        global_step: global_step_number,
        global_step_from_the_latest_canonical_block: global_step_number,
        minimum_step_to_keep: global_step_number.saturating_sub(capacity as u64),
        seeds,
        capacity,
        mining_state_sender: Some(vdf_mining_state_sender),
    }
}

/// return the larger of max_allowed_vdf_fork_steps or num_recall_ranges_in_partition()
/// num_recall_ranges_in_partition() ensures the capacity of VecDeqeue is large enough for the partition.
/// max_allowed_vdf_fork_steps of 60k allows for forks. VDF capacity limits the depth at which a fork can happen. If the fork happens out of the VDF range, the node cannot validate it.
fn calc_capacity(config: &Config) -> usize {
    let capacity_from_config: u64 = num_recall_ranges_in_partition(&config.consensus);

    let max_allowed_vdf_fork_steps = config.consensus.vdf.max_allowed_vdf_fork_steps;

    let capacity = if capacity_from_config < max_allowed_vdf_fork_steps {
        warn!(
            "capacity in config: {} set too low. Overridden with {}",
            capacity_from_config, max_allowed_vdf_fork_steps
        );
        max_allowed_vdf_fork_steps
    } else {
        capacity_from_config
    };

    capacity.try_into().expect("expected u64 to cast to u32")
}

/// Validate the steps from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
pub fn vdf_steps_are_valid(
    pool: &rayon::ThreadPool,
    vdf_info: &VDFLimiterInfo,
    config: &VdfConfig,
    vdf_steps_guard: &VdfStateReadonly,
    reset_seed: H256,
) -> eyre::Result<()> {
    info!(
        "Checking seed {:?} reset_seed {:?}",
        vdf_info.prev_output, reset_seed
    );

    let start = vdf_info.first_step_number();
    let end: u64 = vdf_info.global_step_number;

    match vdf_steps_guard.read().get_steps(ii(start, end)) {
        Ok(steps) => {
            tracing::debug!("Validating VDF steps from VdfStepsReadGuard!");
            if steps != vdf_info.steps {
                warn_mismatches(&steps, &vdf_info.steps);
                return Err(eyre::eyre!("VDF steps are invalid!"));
            } else {
                // Do not need to check last step checkpoints here, were checked in pre validation
                return Ok(())
            }
        },
        Err(err) =>
            tracing::debug!("Error getting steps from VdfStepsReadGuard: {:?} so calculating vdf steps for validation", err.to_string())
    };

    let mut step_hashes = vdf_info.steps.clone();

    // Add the seed from the previous nonce info to the steps
    let previous_seed = vdf_info.prev_output;
    step_hashes.0.insert(0, previous_seed);

    // Make a read only copy for parallel iterating
    let steps = step_hashes.clone();

    // Calculate the step number of the first step in the blocks sequence
    let start_step_number: u64 = vdf_info.global_step_number - vdf_info.steps.len() as u64;

    // We must calculate the checkpoint iterations for each step sequentially
    // because we only have the first and last checkpoint of each step, but we
    // can calculate each of the steps in parallel
    // Limit threads number to avoid overloading the system using configuration limit
    let test: Vec<(H256, Option<H256List>)> = pool.install(|| {
        (0..steps.len() - 1)
            .into_par_iter()
            .map(|i| {
                let mut hasher = Sha256::new();
                let mut salt = U256::from(step_number_to_salt_number(
                    config,
                    start_step_number + i as u64,
                ));
                let mut seed = steps[i];
                let mut checkpoints: Vec<H256> =
                    vec![H256::default(); config.num_checkpoints_in_vdf_step];
                if start_step_number + i as u64 > 0
                    && (start_step_number + i as u64) % config.reset_frequency as u64 == 0
                {
                    // This is a reset step, seed needs to be applied to the previous step
                    info!(
                        "Applying reset seed {:?} to step number {}",
                        reset_seed,
                        start_step_number + i as u64
                    );
                    seed = apply_reset_seed(seed, reset_seed);
                }
                vdf_sha(
                    &mut hasher,
                    &mut salt,
                    &mut seed,
                    config.num_checkpoints_in_vdf_step,
                    config.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                (
                    *checkpoints.last().unwrap(),
                    if i == steps.len() - 2 {
                        // If this is the last step, return the last checkpoint
                        Some(H256List(checkpoints))
                    } else {
                        // Otherwise, return just the seed for the next step
                        None
                    },
                )
            })
            .collect()
    });

    let last_step_checkpoints = test.last().unwrap().1.clone();
    let test: H256List = H256List(test.into_iter().map(|par| par.0).collect());

    let steps_are_valid = test == vdf_info.steps;

    if !steps_are_valid {
        // Compare the original list with the calculated one
        warn_mismatches(&test, &vdf_info.steps);
        return Err(eyre::eyre!("VDF steps are invalid!"));
    }

    let last_step_checkpoints_are_valid = last_step_checkpoints
        .as_ref()
        .is_some_and(|cks| *cks == vdf_info.last_step_checkpoints);

    if !last_step_checkpoints_are_valid {
        // Compare the original list with the calculated one
        if let Some(cks) = last_step_checkpoints {
            warn_mismatches(&cks, &vdf_info.last_step_checkpoints)
        }
        return Err(eyre::eyre!("VDF last step checkpoints are invalid!"));
    }

    Ok(())
}

pub mod test_helpers {
    use super::*;

    use std::sync::RwLock;
    use tokio::sync::mpsc::channel;

    pub fn mocked_vdf_service(config: &Config) -> AtomicVdfState {
        let (vdf_mining_state_sender, _) = channel::<bool>(1);
        let capacity = calc_capacity(config);

        let state = VdfState {
            global_step: 0,
            global_step_from_the_latest_canonical_block: 0,
            minimum_step_to_keep: 0,
            capacity,
            seeds: VecDeque::default(),
            mining_state_sender: Some(vdf_mining_state_sender),
        };
        Arc::new(RwLock::new(state))
    }
}
