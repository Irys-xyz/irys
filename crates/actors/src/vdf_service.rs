use futures::future::Either;
use irys_database::block_header_by_hash;
use irys_types::{
    block_production::Seed, Config, DatabaseProvider, H256List, VDFLimiterInfo, VdfConfig, H256,
    U256,
};
use irys_vdf::{apply_reset_seed, step_number_to_salt_number, vdf_sha, warn_mismatches};
use nodit::{interval::ii, InclusiveInterval, Interval};

use rayon::prelude::*;
use reth::{
    network::metered_poll_nested_stream_with_budget,
    tasks::{shutdown::GracefulShutdown, TaskExecutor},
};
use reth_db::Database;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    pin::pin,
    sync::{Arc, RwLock},
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
    time::{sleep, Duration},
};
use tracing::{info, warn};

use crate::{
    block_index_service::BlockIndexReadGuard, block_tree_service::BlockTreeReadGuard,
    services::Stop,
};

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

/// Messages that the VDF service supports
#[derive(Debug)]
pub enum VdfServiceMessage {
    /// Send the most recent mining step to all the `PartitionMiningActors`
    VdfSeed(Seed),
    /// Retrieve a read only reference to the ledger partition assignments
    GetVdfStateMessage,
    /// pause the VDF thread via mpsc
    StopMiningMessage,
    /// start/resume the VDF thread via mpsc
    StartMiningMessage,
}

#[derive(Debug)]
struct Inner {
    block_tree_read_guard: BlockTreeReadGuard,
    vdf_state: VdfState,
}

#[derive(Debug)]
pub struct VdfService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<VdfServiceMessage>,
    inner: Inner,
}

impl Default for VdfService {
    fn default() -> Self {
        unimplemented!("do not rely on the default implementation of the `VdfService`");
    }
}

impl VdfService {
    /// Spawn a new EMA service
    pub fn spawn_service(
        exec: &TaskExecutor,
        block_tree_read_guard: BlockTreeReadGuard,
        rx: UnboundedReceiver<VdfServiceMessage>,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
        config: &Config,
    ) -> JoinHandle<()> {
        let capacity = calc_capacity(config);
        exec.spawn_critical_with_graceful_shutdown_signal("EMA Service", |shutdown| async move {
            let vdf_service = Self {
                shutdown,
                msg_rx: rx,
                inner: Inner {
                    block_tree_read_guard,
                    vdf_state: VdfState {
                        global_step: 0,
                        seeds: VecDeque::with_capacity(capacity),
                        capacity,
                        mining_state_sender: Some(vdf_mining_state_sender),
                    },
                },
            };
            vdf_service
                .start()
                .await
                .expect("vdf service encountered an irrecoverable error")
        })
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting VDF service");

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await?;
                }
                Either::Left((None, _)) => {
                    tracing::warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdwon");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down VDFS service");
        Ok(())
    }
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: VdfServiceMessage) -> eyre::Result<()> {
        match msg {
            VdfServiceMessage::VdfSeed(seed) => {
                //TODO: do something
                //self.vdf_state.write().unwrap().increment_step(seed.0);
            }
            VdfServiceMessage::GetVdfStateMessage => {
                //TODO: do something
                //VdfStepsReadGuard::new(self.vdf_state.clone())
            }
            VdfServiceMessage::StopMiningMessage => {
                //TODO: do something
                let sender = self
                    .vdf_state
                    .read()
                    .expect("expected to get read lock on vdf state")
                    .mining_state_sender
                    .clone()
                    .expect("expected valid mining_state_sender");

                sender.send(false).await;
            }
            VdfServiceMessage::StartMiningMessage => {
                //TODO: do something
                let sender = self
                    .vdf_state
                    .read()
                    .expect("expected to get read lock on vdf state")
                    .mining_state_sender
                    .clone()
                    .expect("expected valid mining_state_sender");

                sender.send(true).await;
            }
        };
        Ok(())
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

/// Validate the steps from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
///
/// # Arguments
///
/// * `vdf_info` - The Vdf limiter info from the block header to validate.
///
/// # Returns
///
/// - `bool` - `true` if the steps are valid, false otherwise.
pub fn vdf_steps_are_valid(
    vdf_info: &VDFLimiterInfo,
    config: &VdfConfig,
    vdf_steps_guard: VdfStepsReadGuard,
) -> eyre::Result<()> {
    info!(
        "Checking seed {:?} reset_seed {:?}",
        vdf_info.prev_output, vdf_info.seed
    );

    let start = vdf_info.global_step_number - vdf_info.steps.len() as u64 + 1 as u64;
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
            tracing::debug!("Error getting steps from VdfStepsReadGuard: {:?} so calculating vdf steps for validation", err)
    };

    let reset_seed = vdf_info.seed;

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
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.parallel_verification_thread_limit)
        .build()
        .unwrap();
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
                    config.sha_1s_difficulty,
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

// Tests
#[cfg(test)]
mod tests {
    use irys_storage::ii;
    use irys_types::{H256List, NodeConfig, H256};

    use super::*;

    #[actix_rt::test]
    async fn test_vdf() {
        let testnet_config = NodeConfig::testnet().into();
        let service = VdfService::from_capacity(calc_capacity(&testnet_config));
        service.vdf_state.write().unwrap().seeds = VecDeque::with_capacity(4);
        service.vdf_state.write().unwrap().capacity = 4;
        let addr = service.start();

        // Send 8 seeds 1,2..,8 (capacity is 4)
        for i in 0..8 {
            addr.send(VdfServiceMessage::VdfSeed(Seed(H256([(i + 1) as u8; 32]))))
                .await
                .unwrap();
        }

        let state = addr
            .send(VdfServiceMessage::GetVdfStateMessage)
            .await
            .unwrap();

        let steps = state.read().seeds.iter().cloned().collect::<Vec<_>>();

        // Should only contain last 4 seeds
        assert_eq!(steps.len(), 4);

        // Check last 4 seeds are stored
        for i in 0..4 {
            assert_eq!(steps[i], Seed(H256([(i + 5) as u8; 32])));
        }

        // range not stored
        let get_error = state.read().get_steps(ii(3, 5));
        assert!(get_error.is_err());

        // ok inner range
        let get = state.read().get_steps(ii(6, 7)).unwrap();
        assert_eq!(H256List(vec![H256([6; 32]), H256([7; 32])]), get);

        // complete stored range
        let get_all = state.read().get_steps(ii(5, 8)).unwrap();
        assert_eq!(
            H256List(vec![
                H256([5; 32]),
                H256([6; 32]),
                H256([7; 32]),
                H256([8; 32])
            ]),
            get_all
        );
    }
}
