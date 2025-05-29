use std::sync::Arc;

use crate::block_producer::SolutionFoundMessage;
use crate::broadcast_mining_service::{
    BroadcastDifficultyUpdate, BroadcastMiningSeed, BroadcastMiningService,
    BroadcastPartitionsExpiration, Subscribe, Unsubscribe,
};
use crate::packing::PackingRequest;
use crate::vdf_service::VdfStepsReadGuard;
use actix::prelude::*;
use actix::{Actor, Context, Handler, Message};
use eyre::WrapErr;
use irys_efficient_sampling::Ranges;
use irys_storage::{ie, ii, StorageModule};
use irys_types::block_production::Seed;
use irys_types::{block_production::SolutionContext, H256, U256};
use irys_types::{
    partition_chunk_offset_ie, AtomicVdfStepNumber, Config, H256List, LedgerChunkOffset,
    PartitionChunkOffset, PartitionChunkRange,
};
use openssl::sha;
use tracing::{debug, error, info, warn, Span};

#[derive(Debug, Clone)]
pub struct PartitionMiningActor {
    config: Config,
    block_producer_actor: Recipient<SolutionFoundMessage>,
    packing_actor: Recipient<PackingRequest>,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
    difficulty: U256,
    ranges: Ranges,
    steps_guard: VdfStepsReadGuard,
    atomic_global_step_number: AtomicVdfStepNumber,
    span: Span,
}

/// Allows this actor to live in the the local service registry
impl Supervised for PartitionMiningActor {}

impl PartitionMiningActor {
    pub fn new(
        config: &Config,
        block_producer_addr: Recipient<SolutionFoundMessage>,
        packing_actor: Recipient<PackingRequest>,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
        steps_guard: VdfStepsReadGuard,
        atomic_global_step_number: AtomicVdfStepNumber,
        initial_difficulty: U256,
        span: Option<Span>,
    ) -> Self {
        Self {
            config: config.clone(),
            block_producer_actor: block_producer_addr,
            packing_actor,
            ranges: Ranges::new(
                (config.consensus.num_chunks_in_partition
                    / config.consensus.num_chunks_in_recall_range)
                    .try_into()
                    .expect("Recall ranges number exceeds usize representation"),
            ),
            storage_module,
            should_mine: start_mining,
            difficulty: initial_difficulty,
            steps_guard,
            atomic_global_step_number,
            span: span.unwrap_or(Span::current()),
        }
    }

    fn get_recall_range(
        &mut self,
        step: u64,
        seed: &H256,
        partition_hash: &H256,
    ) -> eyre::Result<u64> {
        let span = self.span.clone();
        let _span = span.enter();
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

    fn mine_partition_with_seed(
        &mut self,
        mining_seed: H256,
        vdf_step: u64,
        checkpoints: H256List,
    ) -> eyre::Result<Option<SolutionContext>> {
        let span = self.span.clone();
        let _span = span.enter();
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

        // info!(
        //     "Recall range index {} start chunk index {}",
        //     recall_range_index, start_chunk_offset
        // );

        let read_range = partition_chunk_offset_ie!(
            start_chunk_offset,
            start_chunk_offset + self.config.consensus.num_chunks_in_recall_range as u32
        );

        // haven't tested this, but it looks correct
        let chunks = self.storage_module.read_chunks(read_range)?;
        // debug!(
        //     "Got chunks {} from read range {:?}",
        //     &chunks.len(),
        //     &read_range
        // );

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
                irys_storage::ChunkType::Entropy => (None, None),
                irys_storage::ChunkType::Data => self
                    .storage_module
                    .read_tx_data_path(LedgerChunkOffset::from(*partition_chunk_offset))?,
                irys_storage::ChunkType::Uninitialized => {
                    return Err(eyre::eyre!("Cannot mine uninitialized chunks"))
                }
            };

            // info!(
            //     "partition_hash: {}, chunk offset: {}",
            //     partition_hash, chunk_offset
            // );

            let mut hasher = sha::Sha256::new();
            hasher.update(chunk_bytes);
            hasher.update(&partition_chunk_offset.to_le_bytes());
            hasher.update(mining_seed.as_bytes());
            let solution_hash = hasher.finish();
            let test_solution = hash_to_number(&solution_hash);

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

                let solution = SolutionContext {
                    partition_hash,
                    chunk_offset: *partition_chunk_offset,
                    recall_chunk_index: index as u32,
                    mining_address: self.config.node_config.miner_address(),
                    tx_path, // capacity partitions have no tx_path nor data_path
                    data_path,
                    chunk: chunk_bytes.clone(),
                    vdf_step,
                    checkpoints,
                    seed: Seed(mining_seed),
                    solution_hash: H256::from(solution_hash),
                };

                // TODO: Let all partitions know to stop mining

                // Once solution is sent stop mining and let all other partitions know
                return Ok(Some(solution));
            }
        }

        Ok(None)
    }
}

impl Actor for PartitionMiningActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        let span = self.span.clone();
        let _span = span.enter();

        let broadcaster = BroadcastMiningService::from_registry();
        broadcaster.do_send(Subscribe(ctx.address()));
        debug!("Partition Mining Actor Started");
    }

    fn stopping(&mut self, ctx: &mut Context<Self>) -> Running {
        let broadcaster = BroadcastMiningService::from_registry();
        broadcaster.do_send(Unsubscribe(ctx.address()));
        Running::Stop
    }
}

impl Handler<BroadcastMiningSeed> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMiningSeed, _: &mut Context<Self>) {
        let span = self.span.clone();
        let _span = span.enter();

        let seed = msg.seed;
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

        let lag = current_step - msg.global_step;

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

        match self.mine_partition_with_seed(seed.into_inner(), msg.global_step, msg.checkpoints) {
            Ok(Some(s)) => match self.block_producer_actor.try_send(SolutionFoundMessage(s)) {
                Ok(_) => {
                    // debug!("Solution sent!");
                }
                Err(err) => error!("Error submitting solution to block producer {:?}", err),
            },

            Ok(None) => {
                //debug!("No solution sent!");
            }
            Err(err) => error!("Error in handling mining solution {:?}", err),
        };
    }
}

impl Handler<BroadcastDifficultyUpdate> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, msg: BroadcastDifficultyUpdate, _: &mut Context<Self>) {
        let span = self.span.clone();
        let _span = span.enter();

        let new_diff = msg.0.diff;
        debug!(
            "updating difficulty target in partition miner {}: from {} to {} (diff: {})",
            &self.storage_module.id,
            &self.difficulty,
            &new_diff,
            &self.difficulty.abs_diff(new_diff)
        );
        self.difficulty = new_diff;
    }
}

impl Handler<BroadcastPartitionsExpiration> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, msg: BroadcastPartitionsExpiration, _ctx: &mut Context<Self>) {
        let span = self.span.clone();
        let _span = span.enter();
        self.storage_module.partition_hash().map(|partition_hash| {
            let msg = msg.0;
            if msg.0.contains(&partition_hash) {
                if let Ok(interval) = self.storage_module.reset() {
                    debug!(?partition_hash, ?interval, "Expiring partition hash");
                    self.packing_actor.do_send(PackingRequest {
                        storage_module: self.storage_module.clone(),
                        chunk_range: PartitionChunkRange(interval),
                    });
                } else {
                    error!(
                        ?partition_hash,
                        "Expiring partition hash, could not reset its storage module!"
                    );
                    return Err(eyre::eyre!(
                        "Could not reset storage module with partition hash {}",
                        partition_hash
                    ));
                }
            }
            Ok(())
        });
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
/// Message type for controlling mining
pub struct MiningControl(pub bool);

impl MiningControl {
    const fn into_inner(self) -> bool {
        self.0
    }
}

impl Handler<MiningControl> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, control: MiningControl, _ctx: &mut Context<Self>) -> Self::Result {
        let span = self.span.clone();
        let _span = span.enter();

        let should_mine = control.into_inner();
        debug!(
            "Setting should_mine to {} from {}",
            &self.should_mine, &should_mine
        );
        self.should_mine = should_mine
    }
}

pub fn hash_to_number(hash: &[u8]) -> U256 {
    U256::from_little_endian(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block_producer::{BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage},
        broadcast_mining_service::{BroadcastMiningSeed, BroadcastMiningService},
        mining::{PartitionMiningActor, Seed},
        packing::PackingActor,
        vdf_service::{test_helpers::mocked_vdf_service, VdfServiceMessage},
    };
    use actix::actors::mocker::Mocker;
    use actix::{Actor, Addr, Recipient};
    
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::{ie, PackingParams, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
    use irys_types::{
        block_production::SolutionContext, chunk::UnpackedChunk, partition::PartitionAssignment,
        storage::LedgerChunkRange, StorageSyncConfig, H256,
    };
    use irys_types::{
        ledger_chunk_offset_ie, ConsensusConfig, H256List, IrysBlockHeader, LedgerChunkOffset,
        NodeConfig,
    };
    use reth::payload::EthBuiltPayload;
    use std::any::Any;
    use std::sync::atomic::AtomicU64;
    use std::sync::RwLock;
    use std::time::Duration;
    use tokio::time::sleep;

    fn get_mocked_block_producer(
        closure_arc: Arc<RwLock<Option<SolutionContext>>>,
    ) -> BlockProducerMockActor {
        let closure_arc = closure_arc.clone();
        BlockProducerMockActor::mock(Box::new(move |msg, _ctx| {
            let solution_message: SolutionFoundMessage =
                *msg.downcast::<SolutionFoundMessage>().unwrap();
            let solution = solution_message.0;

            {
                let mut lck = closure_arc.write().unwrap();
                lck.replace(solution);
            }

            let inner_result = None::<(Arc<IrysBlockHeader>, EthBuiltPayload)>;
            Box::new(Some(inner_result)) as Box<dyn Any>
        }))
    }

    #[test_log::test(actix_rt::test)]
    async fn test_solution() {
        let chunk_count = 4;
        let chunk_size = 32;
        let tmp_dir = setup_tracing_and_temp_dir(Some("get_by_data_tx_offset_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: chunk_count.into(),
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chunk_migration_depth: 1, // Testnet / single node config
                chain_id: 1,
                ..ConsensusConfig::testnet()
            }),
            base_directory: base_path.clone(),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);

        let partition_hash = H256::random();
        let chunk_data = [0; 32];
        let data_path = [4, 3, 2, 1];
        let tx_path = [4, 3, 2, 1];
        let rwlock: RwLock<Option<SolutionContext>> = RwLock::new(None);
        let arc_rwlock = Arc::new(rwlock);
        let closure_arc = arc_rwlock.clone();

        let mocked_block_producer = get_mocked_block_producer(closure_arc);

        let packing = Mocker::<PackingActor>::mock(Box::new(move |_msg, _ctx| {
            Box::new(Some(())) as Box<dyn Any>
        }));

        let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
        let recipient: Recipient<SolutionFoundMessage> = block_producer_actor_addr.recipient();
        let mocked_addr = MockedBlockProducerAddr(recipient);

        // Set up the storage geometry for this test
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(0),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (partition_chunk_offset_ie!(0, chunk_count), "hdd0".into()), // 0 to 3 inclusive, 4 chunks
            ],
        }];

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = Arc::new(StorageModule::new(storage_module_info, &config).unwrap());

        // Verify the packing params file was crated in the submodule
        let params_path = base_path.join("hdd0").join("packing_params.toml");
        let params = PackingParams::from_toml(params_path).expect("packing params to load");
        assert_eq!(params.partition_hash, Some(partition_hash));

        // Pack the storage module
        storage_module.pack_with_zeros();

        let path = temporary_directory(None, false);
        let _db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        let data_root = H256::random();
        let data_size = chunk_size * chunk_count;

        let _ = storage_module.index_transaction_data(
            tx_path.to_vec(),
            data_root,
            LedgerChunkRange(ledger_chunk_offset_ie!(0, chunk_count)),
            data_size,
        );

        for tx_chunk_offset in 0..chunk_count {
            let chunk = UnpackedChunk {
                data_root,
                data_size,
                data_path: data_path.to_vec().into(),
                bytes: chunk_data.to_vec().into(),
                tx_offset: tx_chunk_offset.into(),
            };
            storage_module.write_data_chunk(&chunk).unwrap();
        }

        let _ = storage_module.sync_pending_chunks();

        let mining_broadcaster = BroadcastMiningService::new(None);
        let _mining_broadcaster_addr = mining_broadcaster.start();

        let (tx, _vdf_service_handle, _task_manager) = mocked_vdf_service(&config).await;

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        if let Err(e) = tx.send(VdfServiceMessage::GetVdfStateMessage {
            response: oneshot_tx,
        }) {
            panic!("error: {:?}", e);
        };
        let vdf_steps_guard = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard from GetVdfStateMessage message");

        let atomic_global_step_number = Arc::new(AtomicU64::new(1));

        let partition_mining_actor = PartitionMiningActor::new(
            &config,
            mocked_addr.0,
            packing.start().recipient(),
            storage_module,
            true,
            vdf_steps_guard.clone(),
            atomic_global_step_number,
            U256::zero(),
            None,
        );

        let seed: Seed = Seed(H256::random());
        partition_mining_actor
            .start()
            .send(BroadcastMiningSeed {
                seed,
                checkpoints: H256List(vec![]),
                global_step: 1,
            })
            .await
            .unwrap();

        // busypoll the solution context rwlock
        let solution = 'outer: loop {
            match arc_rwlock.try_read() {
                Ok(lck) => {
                    if lck.is_none() {
                        sleep(Duration::from_millis(50)).await;
                    } else {
                        break 'outer lck.as_ref().unwrap().clone();
                    }
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        };

        tokio::task::yield_now().await;

        // now we validate the solution context
        assert_eq!(
            partition_hash, solution.partition_hash,
            "Not expected partition"
        );

        assert!(
            solution.chunk_offset < chunk_count as u32 * 2,
            "Not expected offset"
        );

        assert_eq!(
            config.node_config.miner_address(),
            solution.mining_address,
            "Not expected partition"
        );

        assert_eq!(
            Some(tx_path.to_vec()),
            solution.tx_path,
            "Not expected partition"
        );

        assert_eq!(
            Some(data_path.to_vec()),
            solution.data_path,
            "Not expected partition"
        );
    }

    #[actix_rt::test]
    async fn test_recall_range_reinit() {
        let tmp_dir = setup_tracing_and_temp_dir(Some("get_by_data_tx_offset_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                num_chunks_in_recall_range: 2, // Recall range size is 5 chunks
                ..ConsensusConfig::testnet()
            }),
            base_directory: base_path.clone(),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
            },
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);

        let partition_hash = H256::random();

        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: partition_hash.clone(),
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(0),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (partition_chunk_offset_ie!(0, 10), "hdd0".into()), // 10 chunks
            ],
        }];

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = Arc::new(StorageModule::new(&storage_module_info, &config).unwrap());

        let rwlock: RwLock<Option<SolutionContext>> = RwLock::new(None);
        let arc_rwlock = Arc::new(rwlock);
        let closure_arc = arc_rwlock.clone();
        let mocked_block_producer = get_mocked_block_producer(closure_arc);
        let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
        let recipient: Recipient<SolutionFoundMessage> = block_producer_actor_addr.recipient();
        let mocked_addr = MockedBlockProducerAddr(recipient);

        let (tx, _vdf_service_handle, _task_manager) = mocked_vdf_service(&config).await;

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        if let Err(e) = tx.send(VdfServiceMessage::GetVdfStateMessage {
            response: oneshot_tx,
        }) {
            panic!("error: {:?}", e);
        };
        let vdf_steps_guard = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard from GetVdfStateMessage message");

        let hash: H256 = H256::random();
        for _ in 0..5 {
            // seeds 1 to 5
            if let Err(e) = tx.send(VdfServiceMessage::VdfSeed(Seed(hash))) {
                panic!("error: {:?}", e);
            };
        }
        // reset occurs at step 5
        for _ in 0..2 {
            // seeds 6 and 7
            if let Err(e) = tx.send(VdfServiceMessage::VdfSeed(Seed(hash))) {
                panic!("error: {:?}", e);
            };
        }

        sleep(Duration::from_secs(1)).await;

        let atomic_global_step_number = Arc::new(AtomicU64::new(1));

        let packing = Mocker::<PackingActor>::mock(Box::new(move |_msg, _ctx| {
            Box::new(Some(())) as Box<dyn Any>
        }));

        let mut partition_mining_actor = PartitionMiningActor::new(
            &config,
            mocked_addr.0,
            packing.start().recipient(),
            storage_module,
            false,
            vdf_steps_guard.clone(),
            atomic_global_step_number,
            U256::zero(),
            None,
        );

        let range = partition_mining_actor
            .get_recall_range(7, &hash, &partition_hash)
            .unwrap();

        let mut ranges = Ranges::new(5);
        ranges.get_recall_range(1, &hash, &partition_hash).unwrap();
        ranges.get_recall_range(2, &hash, &partition_hash).unwrap();
        ranges.get_recall_range(3, &hash, &partition_hash).unwrap();
        ranges.get_recall_range(4, &hash, &partition_hash).unwrap();
        ranges.get_recall_range(5, &hash, &partition_hash).unwrap();
        // reset
        ranges.get_recall_range(6, &hash, &partition_hash).unwrap();
        let range2 = ranges.get_recall_range(7, &hash, &partition_hash).unwrap() as u64;

        assert_eq!(range, range2, "Ranges should be equal");
    }
}
