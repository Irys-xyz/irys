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
    Config, H256List, LedgerChunkOffset, PartitionChunkOffset, PartitionChunkRange,
    SendTraced as _, TokioServiceHandle, U256,
    block_production::{Seed, SolutionContext},
    partition_chunk_offset_ie, u256_from_le_bytes,
};
use irys_vdf::state::VdfStateReadonly;
use reth::tasks::shutdown::Shutdown;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{Instrument as _, debug, error, info, warn};

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
}

/// Fresh recall-range rotation state sized for the partition.
fn fresh_ranges(config: &irys_types::ConsensusConfig) -> Ranges {
    Ranges::new(
        num_recall_ranges_in_partition(config)
            .try_into()
            .expect("Recall ranges number exceeds usize representation"),
    )
    .expect("num_recall_ranges_in_partition is always > 0 for a valid ConsensusConfig")
}

impl PartitionMiningServiceInner {
    pub fn new(
        config: &Config,
        service_senders: ServiceSenders,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
        steps_guard: VdfStateReadonly,
        initial_difficulty: U256,
    ) -> Self {
        Self {
            config: config.clone(),
            service_senders,
            storage_module,
            should_mine: start_mining,
            difficulty: initial_difficulty,
            ranges: fresh_ranges(&config.consensus),
            steps_guard,
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

    /// React to an in-place VDF re-anchor after a deep reorg. The seed buffer's
    /// VALUES were rewritten to canonical while the step numbers stayed the same
    /// (see `VdfState::reanchor_seeds`), so the recall-range rotation cached in
    /// `self.ranges` is now derived from poisoned seeds. Because the step numbers
    /// remain consecutive, `get_recall_range`'s gap-driven reconstruction never
    /// fires on its own — so reset the rotation to a fresh state here, forcing the
    /// next seed to reconstruct it from the healed buffer.
    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_reanchored(&mut self) {
        self.ranges = fresh_ranges(&self.config.consensus);
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn get_recall_range(
        &mut self,
        step: u64,
        seed: &irys_types::H256,
        partition_hash: &irys_types::H256,
    ) -> eyre::Result<u64> {
        let next_ranges_step = self.ranges.last_step_num + 1; // next consecutive step expected to be calculated by ranges
        // Fast path ONLY for the exact next consecutive step. Anything else — `step` AHEAD of the
        // iterator (a gap) OR BEHIND it (a VDF re-anchor rewound the steps below the iterator's
        // stateful rotation position, e.g. partition-recovery) — must rebuild the rotation rather
        // than advance it. The previous `>=` fast-pathed every `step <= last_step_num + 1`, which
        // silently computed a WRONG recall range after a re-anchor rewind (the rotation state was
        // for a higher, now-discarded step), making the node mine blocks whose recall range no
        // longer matches its (re-anchored) VDF steps.
        if next_ranges_step == step {
            debug!("Step {} is the next consecutive step", step);
        } else {
            debug!(
                "Non consecutive step {} (iterator at {}) may need to reconstruct ranges",
                step, self.ranges.last_step_num
            );
            // calculate the nearest step lower or equal to step where recall ranges are reinitialized, as this is the step from where ranges will be recalculated
            let reset_step = self.ranges.reset_step(step);
            debug!(
                "Near reset step is {} num recall ranges in partition {}",
                reset_step, self.ranges.num_recall_ranges_in_partition
            );
            // Reinitialize when the iterator cannot incrementally reach `step` from
            // `next_ranges_step`: either the reset boundary is past where the iterator is (a
            // forward gap across a boundary), or the iterator is AHEAD of `step` (a backward
            // re-anchor rewind — its rotation must be rebuilt from the boundary, not advanced).
            let start = if reset_step > next_ranges_step || step < next_ranges_step {
                debug!(
                    "Step {} not incrementally reachable from last processed step {}, reinitializing ranges ...",
                    step, self.ranges.last_step_num
                );
                self.ranges.reinitialize();
                self.ranges.last_step_num = reset_step.saturating_sub(1); // advance last step number calculated by ranges to (reset_step - 1), so ranges next step will be reset_step line
                reset_step
            } else {
                next_ranges_step
            };
            // check if we need to reconstruct steps, that is interval start..=step-1 is not empty
            if start < step {
                debug!("Getting stored steps from ({}..={})", start, step - 1);
                let vdf_steps = self.steps_guard.read();
                let steps = vdf_steps.get_steps(ii(start, step - 1))?; // -1 because last step is calculated in next get_recall_range call, with its corresponding argument seed
                self.ranges.reconstruct(&steps, partition_hash)?;
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

        for (&partition_chunk_offset, (chunk_bytes, chunk_type)) in chunks.iter() {
            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty

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
                    *partition_chunk_offset - start_chunk_offset,
                    self.config.consensus.num_chunks_in_recall_range,
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

        let current_step = self.steps_guard.current_step();

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

    /// Test-only helper to drive the mining loop directly (bypassing the
    /// service event loop) so a sparse recall range with an `Uninitialized`
    /// hole can be exercised.
    pub fn test_mine_partition_with_seed(
        &mut self,
        mining_seed: irys_types::H256,
        vdf_step: u64,
        checkpoints: &H256List,
    ) -> Option<SolutionContext> {
        self.mine_partition_with_seed(mining_seed, vdf_step, checkpoints)
            .expect("test_mine_partition_with_seed failed")
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

        let storage_module_id = inner.storage_module.id;
        let svc = Self {
            shutdown: shutdown_rx,
            state: inner,
            cmd_rx,
            broadcast_rx,
        };

        let handle = runtime_handle.spawn(
            async move {
                svc.start().await;
            }
            .instrument(tracing::info_span!("partition_mining_service", %storage_module_id)),
        );

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
                            MiningBroadcastEvent::Reanchored => {
                                self.state.handle_reanchored();
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::StorageModuleInfo;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, ConsensusOptions, H256, NodeConfig, partition::PartitionAssignment,
    };
    use irys_vdf::state::VdfState;
    use std::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    /// The consumer half of the in-process VDF re-anchor: `handle_reanchored` — the
    /// one-line dispatch target of `MiningBroadcastEvent::Reanchored` in the service
    /// loop — must reset the efficient-sampling rotation so the next recall-range
    /// query REBUILDS from the (corrected) step buffer instead of continuing the
    /// rotation derived from the poisoned seeds. The producer half (buffer swap,
    /// broadcast-after-swap ordering) is pinned in `irys-vdf`, which cannot reach
    /// this wiring.
    #[test]
    fn reanchored_event_rebuilds_recall_ranges_from_corrected_buffer() {
        let tmp_dir = TempDirBuilder::new().build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                // 10 / 2 -> a 5-range rotation
                num_chunks_in_recall_range: 2,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let partition_hash = H256::repeat_byte(0x77);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), "hdd0".into())],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));

        // Step buffer holding the "poisoned" steps 1..=3.
        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        for step in 1..=3_u64 {
            vdf_state
                .write()
                .unwrap()
                .store_step(Seed(H256::repeat_byte(0xA0 + step as u8)), step);
        }

        let (service_senders, _receivers) = ServiceSenders::new();
        let mut inner = PartitionMiningServiceInner::new(
            &config,
            service_senders,
            storage_module,
            false,
            VdfStateReadonly::new(Arc::clone(&vdf_state)),
            U256::zero(),
        );

        // Prime the rotation across the poisoned era (consecutive-step fast path).
        for step in 1..=3_u64 {
            inner.test_get_recall_range(step, H256::repeat_byte(step as u8), partition_hash);
        }
        assert_eq!(inner.ranges.last_step_num, 3);

        // The VDF thread re-anchors the buffer in place (steps 1..=3 rewritten onto
        // canonical seeds) and then broadcasts `Reanchored`.
        let corrected: Vec<H256> = (1..=3_u8).map(|n| H256::repeat_byte(0xB0 + n)).collect();
        vdf_state
            .write()
            .unwrap()
            .reanchor_seeds(corrected.iter().copied().map(Seed).collect());

        inner.handle_reanchored();
        assert_eq!(
            inner.ranges.last_step_num, 0,
            "handle_reanchored must reset the rotation (fresh_ranges) to force a rebuild"
        );

        // The next query must reconstruct from the corrected buffer: byte-for-byte the
        // recall range a fresh rotation fed the corrected steps [1, 3] would make.
        let seed4 = H256::repeat_byte(0x04);
        let got = inner.test_get_recall_range(4, seed4, partition_hash);

        let mut expected_ranges = Ranges::new(5).expect("5 recall ranges");
        expected_ranges
            .reconstruct(&H256List(corrected), &partition_hash)
            .expect("reconstruct from corrected steps");
        let expected = expected_ranges
            .get_recall_range(4, &seed4, &partition_hash)
            .expect("expected pick");

        assert_eq!(
            got,
            u64::try_from(expected).expect("range fits in u64"),
            "post-reanchor recall range must equal a rebuild from the corrected steps"
        );
        assert_eq!(inner.ranges.last_step_num, 4);
    }

    /// A step BEHIND the rotation iterator must rebuild the rotation rather
    /// than fast-path it. The pre-fix `>=` guard fast-pathed every
    /// `step <= last_step_num + 1`, silently computing a recall range from
    /// rotation state that belonged to a later step; the oracle here is a
    /// second service that only ever stepped consecutively to the queried
    /// step, so any stale-state answer mismatches it.
    #[test]
    fn behind_step_rebuilds_rotation_instead_of_fast_pathing() {
        let tmp_dir = TempDirBuilder::new().build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                num_chunks_in_recall_range: 2,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let partition_hash = H256::repeat_byte(0x77);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), "hdd0".into())],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));

        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        for step in 1..=3_u64 {
            vdf_state
                .write()
                .unwrap()
                .store_step(Seed(H256::repeat_byte(0xA0 + step as u8)), step);
        }

        let (service_senders, _receivers) = ServiceSenders::new();
        let mut inner = PartitionMiningServiceInner::new(
            &config,
            service_senders,
            Arc::clone(&storage_module),
            false,
            VdfStateReadonly::new(Arc::clone(&vdf_state)),
            U256::zero(),
        );
        for step in 1..=3_u64 {
            inner.test_get_recall_range(step, H256::repeat_byte(step as u8), partition_hash);
        }
        assert_eq!(inner.ranges.last_step_num, 3);

        // Oracle: an identical service that only ever reached step 2
        // consecutively — the correct answer for (2, seed) by construction.
        let (oracle_senders, _oracle_receivers) = ServiceSenders::new();
        let mut oracle = PartitionMiningServiceInner::new(
            &config,
            oracle_senders,
            storage_module,
            false,
            VdfStateReadonly::new(Arc::clone(&vdf_state)),
            U256::zero(),
        );
        oracle.test_get_recall_range(1, H256::repeat_byte(0x01), partition_hash);
        let expected = oracle.test_get_recall_range(2, H256::repeat_byte(0x02), partition_hash);

        let got = inner.test_get_recall_range(2, H256::repeat_byte(0x02), partition_hash);
        assert_eq!(
            got, expected,
            "a behind-the-iterator step must rebuild the rotation, not answer from stale state"
        );
    }

    /// Regression test for the sparse-map PoA offset bug. `read_chunks` skips
    /// `Uninitialized` offsets, so the mining loop must key the solution's
    /// partition offset off the returned map key, not a dense index. Offset 0
    /// is left `Uninitialized` (a hole at the start of the recall range), so
    /// the first returned chunk is offset 1 at dense index 0 — the old
    /// `start + index` code produced offset 0 and a PoA over the wrong offset.
    /// Difficulty 0 makes the first chunk win.
    #[test]
    fn solution_offset_keys_off_map_over_uninitialized_hole() {
        let tmp_dir = TempDirBuilder::new().build();
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                // single recall range spanning the whole partition → index 0,
                // so get_recall_range needs no VDF reconstruction
                num_chunks_in_recall_range: 10,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let partition_hash = H256::repeat_byte(0x77);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), "hdd0".into())],
        };
        let storage_module =
            Arc::new(StorageModule::new(&info, &config).expect("test storage module"));

        // Pack offsets 1..10 as Entropy, leaving offset 0 Uninitialized (the hole).
        let chunk = vec![0_u8; config.consensus.chunk_size as usize];
        for offset in 1..10_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                chunk.clone(),
                ChunkType::Entropy,
            );
        }
        storage_module
            .force_sync_pending_chunks()
            .expect("sync entropy writes");

        // Sanity: read_chunks skips offset 0, so the first key is 1 at dense index 0.
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ie!(0, 10))
            .expect("read chunks");
        assert!(
            !chunks.contains_key(&PartitionChunkOffset::from(0)),
            "offset 0 must be an Uninitialized hole"
        );
        assert_eq!(
            *chunks.keys().next().expect("a returned chunk"),
            PartitionChunkOffset::from(1),
            "first returned chunk is offset 1"
        );

        let vdf_state = Arc::new(RwLock::new(VdfState::new(
            8,
            0,
            Arc::new(AtomicBool::new(false)),
        )));
        let (service_senders, _receivers) = ServiceSenders::new();
        let mut inner = PartitionMiningServiceInner::new(
            &config,
            service_senders,
            storage_module,
            false,
            VdfStateReadonly::new(Arc::clone(&vdf_state)),
            U256::zero(), // difficulty 0 → the first chunk wins
        );

        let seed = H256::repeat_byte(0x11);
        // step = 1 hits get_recall_range's consecutive fast path (no VDF reads).
        let solution = inner
            .test_mine_partition_with_seed(seed, 1, &H256List::default())
            .expect("a solution at difficulty 0");

        assert_eq!(
            solution.chunk_offset, 1,
            "solution offset must be the true map key (1), not the dense index (0)"
        );
        // The PoA hash must be computed over that same true offset.
        assert_eq!(
            solution.solution_hash,
            irys_types::compute_solution_hash(&chunk, solution.chunk_offset, &seed),
            "solution hash must be over the true offset"
        );
    }
}
