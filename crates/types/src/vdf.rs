use crate::{
    block_production::Seed, BlockIndexReadGuard, Config, DatabaseProvider, H256List,
    VDFLimiterInfo, VdfConfig, H256, U256,
};
use eyre::WrapErr;
use nodit::{interval::ii, InclusiveInterval, Interval};
use openssl::sha;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock, RwLockReadGuard},
};
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Default)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
    /// whether the VDF thread is mining or paused
    pub mining_state_sender: Option<tokio::sync::mpsc::Sender<bool>>,
}

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
        tracing::info!(
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

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
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
    pub async fn read(&self) -> RwLockReadGuard<'_, VdfState> {
        self.0.read().unwrap()
    }

    /// Try to read steps interval pooling a max. of 10 times waiting for interval to be available
    /// TODO @ernius: remove this method usage after VDF validation is done async, vdf steps validation reads VDF steps blocking last steps pushes so the need of this pooling.
    pub async fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        const MAX_RETRIES: i32 = 10;
        for attempt in 0..MAX_RETRIES {
            match self.read().await.get_steps(i) {
                        Ok(c) => return Ok(c),
                        Err(e) =>
                            tracing::warn!("Requested vdf steps range {:?} still unavailable, attempt: {}, reason: {:?}, waiting ...", &i, attempt, e),
                    };
            // should be similar to a yield
            sleep(Duration::from_millis(200)).await;
        }
        Err(eyre::eyre!(
            "Max. retries reached while waiting to get VDF steps!"
        ))
    }
}

/// Validates VDF `last_step_checkpoints` in parallel across available cores.
///
/// Takes a `VDFLimiterInfo` from a block header and verifies each checkpoint by:
/// 1. Getting initial seed from previous vdf step or `prev_output`
/// 2. Applying entropy if at a reset step
/// 3. Computing checkpoints in parallel using configured thread limit
/// 4. Comparing computed results against provided checkpoints
///
/// Returns Ok(()) if checkpoints are valid, Err otherwise with details of mismatches.
pub async fn last_step_checkpoints_is_valid(
    vdf_info: &VDFLimiterInfo,
    config: &VdfConfig,
) -> eyre::Result<()> {
    let mut seed = if vdf_info.steps.len() >= 2 {
        vdf_info.steps[vdf_info.steps.len() - 2]
    } else {
        vdf_info.prev_output
    };
    let mut checkpoint_hashes = vdf_info.last_step_checkpoints.clone();

    // println!("---");
    // for (i, step) in vdf_info.steps.iter().enumerate() {
    //     println!("step{}: {}", i, Base64::from(step.to_vec()));
    // }
    // println!("seed: {}", Base64::from(seed.to_vec()));
    // println!(
    //     "prev_output: {}",
    //     Base64::from(vdf_info.prev_output.to_vec())
    // );

    // println!(
    //     "cp{}: {}",
    //     0,
    //     Base64::from(vdf_info.last_step_checkpoints[0].to_vec())
    // );
    // println!(
    //     "cp{}: {}",
    //     24,
    //     Base64::from(vdf_info.last_step_checkpoints[24].to_vec())
    // );

    let global_step_number: usize = vdf_info
        .global_step_number
        .try_into()
        .wrap_err("Should run in a 64 bits architecture!")?;

    // If the vdf reset happened on this step, apply the entropy to the seed (special case is step 0 that no reset is applied, then the > 1)
    if (global_step_number > 1) && ((global_step_number - 1) % config.reset_frequency == 0) {
        tracing::info!(
            "Applying reset step: {} seed {:?}",
            global_step_number,
            seed
        );
        let reset_seed = vdf_info.seed;
        seed = apply_reset_seed(seed, reset_seed);
    } else {
        tracing::info!(
            "Not applying reset step: {} seed {:?}",
            global_step_number,
            seed
        );
    };

    // Insert the seed at the head of the checkpoint list
    checkpoint_hashes.0.insert(0, seed);
    let cp = checkpoint_hashes.clone();

    // Calculate the starting salt value for checkpoint validation
    let start_salt = U256::from(step_number_to_salt_number(
        config,
        (global_step_number - 1) as u64,
    ));
    let config = config.clone();

    let test = tokio::task::spawn_blocking(move || {
        // Limit threads number to avoid overloading the system using configuration limit
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.parallel_verification_thread_limit)
            .build()
            .unwrap();

        let num_iterations = config.sha_1s_difficulty;
        let test: Vec<H256> = pool.install(|| {
            (0..config.num_checkpoints_in_vdf_step)
                .into_par_iter()
                .map(|i| {
                    let mut salt_buff: [u8; 32] = [0; 32];
                    (start_salt + i).to_little_endian(&mut salt_buff);
                    let mut seed = cp[i];
                    let mut hasher = Sha256::new();

                    for _ in 0..num_iterations {
                        hasher.update(&salt_buff);
                        hasher.update(seed.as_bytes());
                        seed = H256(hasher.finalize_reset().into());
                    }
                    seed
                })
                .collect::<Vec<H256>>()
        });
        test
    })
    .await?;

    // println!("test{}: {}", 0, Base64::from(test[0].to_vec()));
    // println!("test{}: {}", 24, Base64::from(test[24].to_vec()));

    let is_valid = test == vdf_info.last_step_checkpoints;

    if !is_valid {
        // Compare the blocks list with the calculated one, looking for mismatches
        warn_mismatches(&vdf_info.last_step_checkpoints, &H256List(test));
        Err(eyre::eyre!("Checkpoints are invalid"))
    } else {
        Ok(())
    }
}

pub fn warn_mismatches(a: &H256List, b: &H256List) {
    let mismatches: Vec<(usize, (&H256, &H256))> =
        a.0.iter()
            .zip(&(b.0))
            .enumerate()
            .filter(|(_i, (a, b))| a != b)
            .collect();

    for (index, (a, b)) in mismatches {
        tracing::error!(
            "Mismatched hashes at index {}: expected {:?} got {:?}",
            index,
            a,
            b
        );
    }
}

/// Derives a salt value from the `step_number` for checkpoint hashing
///
/// # Arguments
///
/// * `step_number` - The step the checkpoint belongs to, add 1 to the salt for
/// each subsequent checkpoint calculation.
pub const fn step_number_to_salt_number(config: &VdfConfig, step_number: u64) -> u64 {
    match step_number {
        0 => 0,
        _ => (step_number - 1) * config.num_checkpoints_in_vdf_step as u64 + 1,
    }
}

/// Takes a checkpoint seed and applies the SHA256 block hash seed to it as
/// entropy. First it SHA256 hashes the `reset_seed` then SHA256 hashes the
/// output together with the `seed` hash.
///
/// # Arguments
///
/// * `seed` - The bytes of a SHA256 checkpoint hash
/// * `reset_seed` - The bytes of a SHA256 block hash used as entropy
///
/// # Returns
///
/// A new SHA256 seed hash containing the `reset_seed` entropy to use for
/// calculating checkpoints after the reset.
pub fn apply_reset_seed(seed: H256, reset_seed: H256) -> H256 {
    // Merge the current seed with the SHA256 has of the block hash.
    let mut hasher = sha::Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(reset_seed.as_bytes());
    H256::from(hasher.finish())
}

/// return the larger of MINIMUM_CAPACITY or number of seeds required for (chunks in partition / chunks in recall range)
/// This ensure the capacity of VecDeqeue is large enough for the partition.
pub fn calc_capacity(config: &Config) -> usize {
    const MINIMUM_CAPACITY: u64 = 10_000;
    let capacity_from_config: u64 =
        config.consensus.num_chunks_in_partition / config.consensus.num_chunks_in_recall_range;
    let capacity = if capacity_from_config < MINIMUM_CAPACITY {
        tracing::warn!(
            "capacity in config: {} set too low. Overridden with {}",
            capacity_from_config,
            MINIMUM_CAPACITY
        );
        MINIMUM_CAPACITY
    } else {
        std::cmp::max(MINIMUM_CAPACITY, capacity_from_config)
    };

    capacity.try_into().expect("expected u64 to cast to u32")
}
