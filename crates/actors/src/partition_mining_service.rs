use crate::{
    block_producer::BlockProducerCommand,
    metrics,
    mining_bus::{
        BroadcastDifficultyUpdate, BroadcastMiningSeed, BroadcastPartitionsExpiration,
        MiningBroadcastEvent,
    },
    packing_service::PackingRequest,
    services::ServiceSenders,
};
use eyre::WrapErr as _;
use irys_domain::{ChunkType, StorageModule};
use irys_efficient_sampling::{Ranges, num_recall_ranges_in_partition};
use irys_storage::ii;
use irys_types::{
    AtomicVdfStepNumber, Config, H256List, LedgerChunkOffset, PartitionChunkOffset,
    PartitionChunkRange, SendTraced as _, TokioServiceHandle, U256,
    block_production::{Seed, SolutionContext},
    partition_chunk_offset_ie, u256_from_le_bytes,
};
use irys_vdf::state::VdfStateReadonly;
use reth::tasks::shutdown::Shutdown;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{debug, error, info, warn};

/// Commands that control the partition mining service
#[derive(Debug)]
pub enum PartitionMiningCommand {
    /// Enable/disable mining for this partition
    SetMining(bool),
}

/// Controller for a running PartitionMiningService
#[derive(Debug, Clone)]
pub struct PartitionMiningController {
    cmd_tx: UnboundedSender<PartitionMiningCommand>,
}

impl PartitionMiningController {
    /// Toggle mining on or off for this partition
    pub fn set_mining(&self, enabled: bool) {
        // Best-effort. If the service is gone, ignore the error.
        let _ = self.cmd_tx.send(PartitionMiningCommand::SetMining(enabled));
    }
}

/// Inner logic for the partition mining tokio service
#[derive(Debug)]
pub struct PartitionMiningServiceInner {
    config: Config,
    service_senders: ServiceSenders,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
    difficulty: U256,
    ranges: Ranges,
    steps_guard: VdfStateReadonly,
    atomic_global_step_number: AtomicVdfStepNumber,
}

impl PartitionMiningServiceInner {
    pub fn new(
        config: &Config,
        service_senders: ServiceSenders,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
        steps_guard: VdfStateReadonly,
        atomic_global_step_number: AtomicVdfStepNumber,
        initial_difficulty: U256,
    ) -> Self {
        Self {
            config: config.clone(),
            service_senders,
            storage_module,
            should_mine: start_mining,
            difficulty: initial_difficulty,
            ranges: Ranges::new(
                num_recall_ranges_in_partition(&config.consensus)
                    .try_into()
                    .expect("Recall ranges number exceeds usize representation"),
            ),
            steps_guard,
            atomic_global_step_number,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn set_mining(&mut self, should_mine: bool) {
        debug!(
            "Setting should_mine to {} from {}",
            should_mine, self.should_mine
        );
        self.should_mine = should_mine;
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn update_difficulty(&mut self, new_diff: U256) {
        debug!(
            "Updating difficulty target in partition miner {}: from {} to {} (diff: {})",
            self.storage_module.id,
            self.difficulty,
            new_diff,
            self.difficulty.abs_diff(new_diff)
        );
        self.difficulty = new_diff;
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_partitions_expiration(&mut self, expired: &H256List) {
        if let Some(partition_hash) = self.storage_module.partition_hash()
            && expired.0.contains(&partition_hash)
        {
            if let Ok(interval) = self.storage_module.reset() {
                debug!(
                    storage_module.partition_hash = ?partition_hash,
                    storage_module.packing_interval = ?interval,
                    "Expiring partition hash"
                );
                if let Ok(req) =
                    PackingRequest::new(self.storage_module.clone(), PartitionChunkRange(interval))
                {
                    match self.service_senders.packing_sender.try_send(req) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            warn!(
                                storage_module.id = %self.storage_module.id,
                                storage_module.partition_hash = ?partition_hash,
                                storage_module.packing_interval = ?interval,
                                "Dropping packing request due to a saturated channel"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_req)) => {
                            error!(
                                storage_module.id = %self.storage_module.id,
                                storage_module.partition_hash = ?partition_hash,
                                storage_module.packing_interval = ?interval,
                                "Packing channel closed; failed to enqueue repacking request"
                            );
                        }
                    }
                }
            } else {
                error!(
                    storage_module.partition_hash = ?partition_hash,
                    "Expiring partition hash, could not reset its storage module!"
                );
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn get_recall_range(
        &mut self,
        step: u64,
        seed: &irys_types::H256,
        partition_hash: &irys_types::H256,
    ) -> eyre::Result<u64> {
        let next_ranges_step = self.ranges.last_step_num + 1; // next consecutive step expected to be calculated by ranges
        if next_ranges_step >= step {
            debug!("Step {} already processed or next consecutive one", step);
        } else {
            debug!(
                "Non consecutive step {} may need to reconstruct ranges",
                step
            );
            // calculate the nearest step lower or equal to step where recall ranges are reinitialized, as this is the step from where ranges will be recalculated
            let reset_step = self.ranges.reset_step(step);
            debug!(
                "Near reset step is {} num recall ranges in partition {}",
                reset_step, self.ranges.num_recall_ranges_in_partition
            );
            let start = if reset_step > next_ranges_step {
                debug!(
                    "Step {} is too far ahead of last processed step {}, reinitializing ranges ...",
                    step, self.ranges.last_step_num
                );
                self.ranges.reinitialize();
                self.ranges.last_step_num = reset_step - 1; // advance last step number calculated by ranges to (reset_step - 1), so ranges next step will be reset_step line
                reset_step
            } else {
                next_ranges_step
            };
            // check if we need to reconstruct steps, that is interval start..=step-1 is not empty
            if start < step {
                debug!("Getting stored steps from ({}..={})", start, step - 1);
                let vdf_steps = self.steps_guard.read();
                let steps = vdf_steps.get_steps(ii(start, step - 1))?; // -1 because last step is calculated in next get_recall_range call, with its corresponding argument seed
                self.ranges.reconstruct(&steps, partition_hash);
            };
        }

        u64::try_from(self.ranges.get_recall_range(step, seed, partition_hash)?)
            .wrap_err("recall range larger than u64")
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn mine_partition_with_seed(
        &mut self,
        mining_seed: irys_types::H256,
        vdf_step: u64,
        checkpoints: &H256List,
    ) -> eyre::Result<Option<SolutionContext>> {
        let partition_hash = match self.storage_module.partition_hash() {
            Some(p) => p,
            None => {
                warn!("No partition assigned!");
                return Ok(None);
            }
        };

        // Pick a random recall range in the partition using efficient sampling
        let recall_range_index = self.get_recall_range(vdf_step, &mining_seed, &partition_hash)?;

        // Starting chunk index within partition
        let start_chunk_offset = (recall_range_index as u32)
            .saturating_mul(self.config.consensus.num_chunks_in_recall_range as u32);

        let read_range = partition_chunk_offset_ie!(
            start_chunk_offset,
            start_chunk_offset + self.config.consensus.num_chunks_in_recall_range as u32
        );

        let chunks = self.storage_module.read_chunks(read_range)?;
        if chunks.is_empty() {
            warn!(
                "No chunks found - storage_module_id:{} {}-{}",
                self.storage_module.id,
                &read_range.start(),
                &read_range.end()
            );
        }

        for (index, (_chunk_offset, (chunk_bytes, chunk_type))) in chunks.iter().enumerate() {
            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty
            let partition_chunk_offset =
                PartitionChunkOffset::from(start_chunk_offset + index as u32);

            // Only include the tx_path and data_path for chunks that contain data
            let (tx_path, data_path) = match chunk_type {
                ChunkType::Entropy => (None, None),
                ChunkType::Data => self
                    .storage_module
                    .read_tx_data_path(LedgerChunkOffset::from(*partition_chunk_offset))?,
                ChunkType::Uninitialized => {
                    return Err(eyre::eyre!("Cannot mine uninitialized chunks"));
                }
                ChunkType::Interrupted => {
                    return Err(eyre::eyre!("Cannot mine interrupted chunks"));
                }
            };

            let solution_hash = irys_types::compute_solution_hash(
                chunk_bytes,
                *partition_chunk_offset,
                &mining_seed,
            );
            let test_solution = u256_from_le_bytes(&solution_hash.0);

            if test_solution >= self.difficulty {
                info!(
                    "Solution Found - partition_id: {}, ledger_offset: {}/{}, range_offset: {}/{} difficulty {}",
                    self.storage_module.id,
                    partition_chunk_offset,
                    self.config.consensus.num_chunks_in_partition,
                    index,
                    chunks.len(),
                    self.difficulty
                );
                metrics::record_mining_solution_found();

                let solution = SolutionContext {
                    partition_hash,
                    chunk_offset: *partition_chunk_offset,
                    mining_address: self.config.node_config.miner_address(),
                    tx_path,
                    data_path,
                    chunk: chunk_bytes.clone(),
                    vdf_step,
                    checkpoints: checkpoints.clone(),
                    seed: Seed(mining_seed),
                    solution_hash,
                };

                // Once solution is sent stop mining and let all other partitions know
                return Ok(Some(solution));
            }
        }

        Ok(None)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_seed(&mut self, msg: &BroadcastMiningSeed) {
        let seed = msg.seed.clone();
        if !self.should_mine {
            debug!("Mining disabled, skipping seed {:?}", seed);
            return;
        }

        if self.storage_module.partition_assignment().is_none() {
            debug!("No partition assigned - skipping seed {:?}", seed);
            return;
        }

        let current_step = self
            .atomic_global_step_number
            .load(std::sync::atomic::Ordering::Relaxed);

        debug!(
            "Mining partition {} with seed {:?} step number {} current step {}",
            self.storage_module.partition_hash().unwrap(),
            seed,
            msg.global_step,
            current_step
        );

        let lag = current_step.saturating_sub(msg.global_step);
        if lag >= 3 {
            warn!(
                "Storage module {} is {} steps behind in mining. Skipping.",
                self.storage_module.id, lag
            );
            return;
        }

        debug!(
            "Partition {} -- looking for solution with difficulty >= {}",
            self.storage_module.partition_hash().unwrap(),
            self.difficulty
        );

        match self.mine_partition_with_seed(seed.into_inner(), msg.global_step, &msg.checkpoints) {
            Ok(Some(s)) => {
                let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
                let cmd = BlockProducerCommand::SolutionFound {
                    solution: s,
                    response: response_tx,
                };
                if let Err(err) = self.service_senders.block_producer.send_traced(cmd) {
                    error!(
                        "Error submitting solution to block producer for storage_module {} partition_hash {:?} step {}: {:?}",
                        self.storage_module.id,
                        self.storage_module.partition_hash(),
                        msg.global_step,
                        err
                    );
                }
            }
            Ok(None) => {
                // No solution found this step.
            }
            Err(err) => error!(
                "Error in handling mining solution for storage_module {} partition_hash {:?}: {:?}",
                self.storage_module.id,
                self.storage_module.partition_hash(),
                err
            ),
        };
    }
}

#[cfg(test)]
impl PartitionMiningServiceInner {
    /// Test-only helper to expose recall-range computation without starting the service.
    pub fn test_get_recall_range(
        &mut self,
        step: u64,
        seed: irys_types::H256,
        partition_hash: irys_types::H256,
    ) -> u64 {
        self.get_recall_range(step, &seed, &partition_hash)
            .expect("test_get_recall_range failed")
    }
}

/// Tokio service for partition mining
#[derive(Debug)]
pub struct PartitionMiningService {
    shutdown: Shutdown,
    state: PartitionMiningServiceInner,
    cmd_rx: UnboundedReceiver<PartitionMiningCommand>,
    broadcast_rx: UnboundedReceiver<Arc<MiningBroadcastEvent>>,
}

impl PartitionMiningService {
    /// Spawns a Tokio partition mining service and subscribes to the global broadcaster
    pub fn spawn_service(
        inner: PartitionMiningServiceInner,
        runtime_handle: tokio::runtime::Handle,
    ) -> (PartitionMiningController, TokioServiceHandle) {
        // Control channel
        let (cmd_tx, cmd_rx) = unbounded_channel();

        // Broadcast subscription channel
        let broadcast_rx = inner.service_senders.subscribe_mining_broadcast();

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let svc = Self {
            shutdown: shutdown_rx,
            state: inner,
            cmd_rx,
            broadcast_rx,
        };

        let handle = runtime_handle.spawn(async move {
            svc.start().await;
        });

        let controller = PartitionMiningController { cmd_tx };

        let svc_handle = TokioServiceHandle {
            name: "partition_mining_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        };

        (controller, svc_handle)
    }

    async fn start(mut self) {
        info!("Starting partition mining service");
        loop {
            tokio::select! {
                // Shutdown
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for partition mining service");
                    break;
                }

                // Control commands
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(PartitionMiningCommand::SetMining(enabled)) => {
                            self.state.set_mining(enabled);
                        }
                        None => {
                            warn!("PartitionMiningService controller channel closed; stopping service");
                            break;
                        }
                    }
                }

                // Broadcast messages (seed/diff/expiration)
                evt = self.broadcast_rx.recv() => {
                    match evt {
                        Some(arc_evt) => match arc_evt.as_ref() {
                            MiningBroadcastEvent::Seed(msg) => {
                                self.state.handle_seed(msg);
                            }
                            MiningBroadcastEvent::Difficulty(BroadcastDifficultyUpdate(h)) => {
                                self.state.update_difficulty(h.diff);
                            }
                            MiningBroadcastEvent::PartitionsExpiration(BroadcastPartitionsExpiration(list)) => {
                                self.state.handle_partitions_expiration(list);
                            }
                        },
                        None => {
                            warn!("Mining broadcaster channel closed; stopping service");
                            break;
                        }
                    }
                }
            }
        }
        info!("Partition mining service stopped");
    }
}
