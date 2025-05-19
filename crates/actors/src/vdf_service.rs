use futures::future::Either;
use irys_database::{block_header_by_hash, BlockIndex};
use irys_efficient_sampling::num_recall_ranges_in_partition;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_types::{
    block_production::Seed, Config, DatabaseProvider, H256List, VDFLimiterInfo, VdfConfig, H256,
    U256,
};
use irys_vdf::{apply_reset_seed, step_number_to_salt_number, vdf_sha, warn_mismatches};
use nodit::{interval::ii, InclusiveInterval, Interval};
use rayon::prelude::*;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use reth_db::Database;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    pin::pin,
    sync::{Arc, RwLock, RwLockReadGuard},
};
use tokio::{
    sync::mpsc::UnboundedReceiver,
    task::JoinHandle,
    time::{sleep, Duration},
};
use tracing::{info, warn};

use crate::block_index_service::BlockIndexReadGuard;

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
    GetVdfStateMessage {
        response: tokio::sync::oneshot::Sender<VdfStepsReadGuard>,
    },
    /// pause the VDF thread via mpsc
    StopMiningMessage,
    /// start/resume the VDF thread via mpsc
    StartMiningMessage,
}

#[derive(Debug)]
struct Inner {
    block_index_read_guard: BlockIndexReadGuard,
    vdf_state: AtomicVdfState,
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
    /// Spawn a new VDF service
    pub fn spawn_service(
        exec: &TaskExecutor,
        irys_db: DatabaseProvider,
        block_index_read_guard: BlockIndexReadGuard,
        rx: UnboundedReceiver<VdfServiceMessage>,
        vdf_mining_state_sender: tokio::sync::mpsc::Sender<bool>,
        config: &Config,
    ) -> JoinHandle<()> {
        let vdf_state = create_state(
            block_index_read_guard.clone(),
            irys_db,
            vdf_mining_state_sender,
            config,
        );
        exec.spawn_critical_with_graceful_shutdown_signal("VDF Service", |shutdown| async move {
            let vdf_service = Self {
                shutdown,
                msg_rx: rx,
                inner: Inner {
                    block_index_read_guard,
                    vdf_state: Arc::new(RwLock::new(vdf_state)),
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
                self.vdf_state.write().unwrap().increment_step(seed);
            }
            VdfServiceMessage::GetVdfStateMessage { response } => {
                let guard = VdfStepsReadGuard::new(self.vdf_state.clone());
                if let Err(e) = response.send(guard) {
                    tracing::error!("response.send(guard) error: {:?}", e);
                };
            }
            VdfServiceMessage::StopMiningMessage => {
                let sender = self
                    .vdf_state
                    .read()
                    .expect("expected to get read lock on vdf state")
                    .mining_state_sender
                    .clone()
                    .expect("expected valid mining_state_sender");

                if let Err(e) = sender.send(false).await {
                    tracing::error!("failed to send false to mining_state_sender: {:?}", e);
                }
            }
            VdfServiceMessage::StartMiningMessage => {
                let sender = self
                    .vdf_state
                    .read()
                    .expect("expected to get read lock on vdf state")
                    .mining_state_sender
                    .clone()
                    .expect("expected valid mining_state_sender");

                if let Err(e) = sender.send(true).await {
                    tracing::error!("failed to send true to mining_state_sender: {:?}", e);
                }
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

pub mod test_helpers {
    use super::*;
    use crate::{
        block_tree_service::{BlockState, BlockTreeCache, BlockTreeReadGuard, ChainState},
        vdf_service::{VdfService, VdfServiceMessage},
    };

    use irys_types::IrysBlockHeader;
    use irys_types::H256;
    use reth::{core::args, tasks::TaskManager};
    use std::sync::RwLock;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

    pub async fn mocked_vdf_service(
        config: &Config,
    ) -> (
        UnboundedSender<VdfServiceMessage>,
        JoinHandle<()>,
        TaskManager,
        TaskExecutor,
    ) {
        //setup mock blocks and tree
        let height_chain_max = 15;
        let max_confirmed_height = height_chain_max / 2;
        let mut blocks = (0..=height_chain_max)
            .map(|height| {
                let block = IrysBlockHeader {
                    height,
                    ..IrysBlockHeader::new_mock_header()
                };

                // Determine chain state based on block height
                let state = if block.height <= max_confirmed_height {
                    ChainState::Onchain
                } else {
                    ChainState::NotOnchain(BlockState::ValidationScheduled)
                };

                (block, state)
            })
            .collect::<Vec<_>>();
        // prep to spawn VDF service
        // this is so we can send it new VDF steps as part of this test
        let task_manager = TaskManager::new(tokio::runtime::Handle::current());
        let task_executor = task_manager.executor();
        let (tx, rx) = unbounded_channel();
        let (vdf_mining_state_sender, _) = tokio::sync::mpsc::channel::<bool>(1);

        let block_index: Arc<RwLock<BlockIndex>> = Arc::new(RwLock::new(
            BlockIndex::new(&config.node_config).await.unwrap(),
        ));

        let block_index_guard = BlockIndexReadGuard::new(block_index);

        let irys_db_env =
            open_or_create_irys_consensus_data_db(&config.node_config.irys_consensus_data_dir());
        let irys_db = DatabaseProvider(Arc::new(irys_db_env.expect("expected valid irys_db_env")));

        // spawn VDF service
        // this is so we can send it new VDF steps as part of this test
        let vdf_service_handle = VdfService::spawn_service(
            &task_executor,
            irys_db,
            block_index_guard.clone(),
            rx,
            vdf_mining_state_sender,
            &config,
        );

        (tx, vdf_service_handle, task_manager, task_executor)
    }
}

// Tests
#[cfg(test)]
mod tests {
    use crate::vdf_service::test_helpers::mocked_vdf_service;
    use irys_storage::ii;
    use irys_types::{H256List, NodeConfig, H256};

    use super::*;

    #[actix_rt::test]
    /// Tests vdf dequeue populates via FIFO and shows steps being dropped.
    async fn test_vdf() {
        let mut node_config = NodeConfig::testnet();
        // set queue to length 4 with 8/2 occurring within the vdf spawn
        node_config.consensus.get_mut().num_chunks_in_partition = 8;
        node_config.consensus.get_mut().num_chunks_in_recall_range = 2;
        // set queue to length 4 so old steps are discarded FIFO
        node_config
            .consensus
            .get_mut()
            .vdf
            .max_allowed_vdf_fork_steps = 4;
        let testnet_config: Config = node_config.into();

        // start service senders/receivers
        let (tx, _vdf_service_handle, _task_manager, _task_executor) =
            mocked_vdf_service(&testnet_config).await;

        // Send 8 seeds 1,2..,8 (capacity is 4)
        for i in 0..8 {
            if let Err(e) = tx.send(VdfServiceMessage::VdfSeed(Seed(H256([(i + 1) as u8; 32])))) {
                panic!("error sending VdfServiceMessage::VdfSeed: {:?}", e);
            }
        }

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        if let Err(e) = tx.send(VdfServiceMessage::GetVdfStateMessage {
            response: oneshot_tx,
        }) {
            panic!(
                "error sending VdfServiceMessage::GetVdfStateMessage: {:?}",
                e
            );
        }
        let state = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard from GetVdfStateMessage message");

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
