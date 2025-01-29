use crate::block_producer::SolutionFoundMessage;
use crate::broadcast_mining_service::{
    BroadcastDifficultyUpdate, BroadcastMiningSeed, BroadcastMiningService, Subscribe, Unsubscribe,
};
use crate::vdf_service::{AtomicVdfState, VdfState, VdfStepsReadGuard};
use actix::prelude::*;
use actix::{Actor, Context, Handler, Message};
use irys_efficient_sampling::Ranges;
use irys_storage::{ie, ii, StorageModule};
use irys_types::app_state::DatabaseProvider;
use irys_types::block_production::Seed;
use irys_types::{block_production::SolutionContext, H256, U256};
use irys_types::{Address, H256List, PartitionChunkOffset};
use openssl::sha;
use std::sync::{Arc, RwLock};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct PartitionMiningActor {
    mining_address: Address,
    _database_provider: DatabaseProvider,
    block_producer_actor: Recipient<SolutionFoundMessage>,
    storage_module: Arc<StorageModule>,
    vdf_state: AtomicVdfState,
    should_mine: bool,
    difficulty: U256,
    ranges: Ranges,
    steps_guard: VdfStepsReadGuard,
}

/// Allows this actor to live in the the local service registry
impl Supervised for PartitionMiningActor {}

impl PartitionMiningActor {
    pub fn new(
        mining_address: Address,
        _database_provider: DatabaseProvider,
        block_producer_addr: Recipient<SolutionFoundMessage>,
        vdf_state: AtomicVdfState,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
        steps_guard: VdfStepsReadGuard,
    ) -> Self {
        Self {
            mining_address,
            _database_provider,
            block_producer_actor: block_producer_addr,
            vdf_state: vdf_state,
            ranges: Ranges::new(
                (storage_module.storage_config.num_chunks_in_partition
                    / storage_module.storage_config.num_chunks_in_recall_range)
                    .try_into()
                    .expect("Recall ranges number exceeds usize representation"),
            ),
            storage_module,
            should_mine: start_mining,
            difficulty: U256::zero(),
            steps_guard,
        }
    }

    fn get_recall_range(
        &mut self,
        step: u64,
        seed: &H256,
        partition_hash: &H256,
    ) -> eyre::Result<u64> {
        let vdf_steps = self.steps_guard.read();
        if self.ranges.last_step_num + 1 >= step {
            debug!("Step {} already processed or next consecutive one", step);
            Ok(self.ranges.get_recall_range(step, seed, partition_hash) as u64)
        } else {
            // This code is not needed for just one node, but will be needed for multiple nodes
            warn!("Non consecutive Step {} need to reconstruct ranges", step);
            let steps = vdf_steps.get_steps(ii(self.ranges.last_step_num + 1, step - 1))?;
            self.ranges.reconstruct(&steps, partition_hash);
            Ok(self.ranges.get_recall_range(step, seed, partition_hash) as u64)
        }
    }

    fn mine_partition_with_seed(
        &mut self,
        mining_seed: H256,
        vdf_step: u64,
        checkpoints: H256List,
    ) -> eyre::Result<Option<SolutionContext>> {
        let partition_hash = match self.storage_module.partition_hash() {
            Some(p) => p,
            None => {
                warn!("No partition assigned!");
                return Ok(None);
            }
        };

        // Pick a random recall range in the partition using efficient sampling
        let recall_range_index =
            { self.get_recall_range(vdf_step, &mining_seed, &partition_hash)? };

        let config = &self.storage_module.storage_config;

        // Starting chunk index within partition
        let start_chunk_offset =
            recall_range_index as u32 * config.num_chunks_in_recall_range as u32;

        // info!(
        //     "Recall range index {} start chunk index {}",
        //     recall_range_index, start_chunk_offset
        // );

        let read_range = ie(
            start_chunk_offset as u32,
            start_chunk_offset + config.num_chunks_in_recall_range as u32,
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
                "No chunks found - storage_module_id:{}",
                self.storage_module.id
            );
        }

        for (index, (_chunk_offset, (chunk_bytes, chunk_type))) in chunks.iter().enumerate() {
            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty
            let partition_chunk_offset =
                (start_chunk_offset + index as u32) as PartitionChunkOffset;

            // Only include the tx_path and data_path for chunks that contain data
            let (tx_path, data_path) = match chunk_type {
                irys_storage::ChunkType::Entropy => (None, None),
                irys_storage::ChunkType::Data => self
                    .storage_module
                    .read_tx_data_path(partition_chunk_offset as u64)?,
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
                    "Solution Found - partition_id: {}, ledger_offset: {}/{}, range_offset: {}/{}",
                    self.storage_module.id,
                    partition_chunk_offset,
                    config.num_chunks_in_partition,
                    index,
                    chunks.len()
                );

                let solution = SolutionContext {
                    partition_hash,
                    chunk_offset: partition_chunk_offset,
                    recall_chunk_index: index as u32,
                    mining_address: self.mining_address,
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
        let broadcaster = BroadcastMiningService::from_registry();
        broadcaster.do_send(Subscribe(ctx.address()));
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
        let seed = msg.seed;
        if !self.should_mine {
            debug!("Mining disabled, skipping seed {:?}", seed);
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
        let new_diff = msg.0.diff;
        debug!(
            "updating difficulty target: from {} to {} (diff: {})",
            &self.difficulty,
            &new_diff,
            &self.difficulty.abs_diff(new_diff)
        );
        self.difficulty = new_diff;
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
    use crate::block_producer::{
        BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage,
    };
    use crate::broadcast_mining_service::{BroadcastMiningSeed, BroadcastMiningService};
    use crate::mining::{PartitionMiningActor, Seed};
    use crate::vdf_service::{GetVdfStateMessage, VdfService, VdfStepsReadGuard};
    use actix::{Actor, Addr, ArbiterService, Recipient};
    use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::{
        ie, initialize_storage_files, read_info_file, StorageModule, StorageModuleInfo,
    };
    use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
    use irys_types::{
        app_state::DatabaseProvider, block_production::SolutionContext, chunk::UnpackedChunk,
        partition::PartitionAssignment, storage::LedgerChunkRange, Address, StorageConfig, H256,
    };
    use irys_types::{H256List, IrysBlockHeader};
    use std::any::Any;
    use std::sync::RwLock;
    use std::time::Duration;
    use tokio::time::sleep;

    #[actix_rt::test]
    async fn test_solution() {
        let partition_hash = H256::random();
        let mining_address = Address::random();
        let chunk_count = 4;
        let chunk_size = 32;
        let chunk_data = [0; 32];
        let data_path = [4, 3, 2, 1];
        let tx_path = [4, 3, 2, 1];
        let rwlock: RwLock<Option<SolutionContext>> = RwLock::new(None);
        let arc_rwlock = Arc::new(rwlock);
        let closure_arc = arc_rwlock.clone();

        let mocked_block_producer = BlockProducerMockActor::mock(Box::new(move |msg, _ctx| {
            let solution_message: SolutionFoundMessage =
                *msg.downcast::<SolutionFoundMessage>().unwrap();
            let solution = solution_message.0;

            {
                let mut lck = closure_arc.write().unwrap();
                lck.replace(solution);
            }

            let inner_result = None::<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>;
            Box::new(Some(inner_result)) as Box<dyn Any>
        }));

        let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
        let recipient: Recipient<SolutionFoundMessage> = block_producer_actor_addr.recipient();
        let mocked_addr = MockedBlockProducerAddr(recipient);

        //SystemRegistry::set(block_producer_actor_addr);

        // Set up the storage geometry for this test
        let storage_config = StorageConfig {
            chunk_size,
            num_chunks_in_partition: chunk_count.into(),
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: mining_address,
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1,
            chunk_migration_depth: 1, // Testnet / single node config
        };

        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: mining_address,
                ledger_id: Some(0),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (ie(0, chunk_count), "hdd0".into()), // 0 to 3 inclusive, 4 chunks
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("storage_module_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let _ = initialize_storage_files(&base_path, &infos, &vec![]);

        // Verify the StorageModuleInfo file was crated in the base path
        let file_infos = read_info_file(&base_path.join("StorageModule_0.json")).unwrap();
        assert_eq!(file_infos, infos[0]);

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module =
            Arc::new(StorageModule::new(&base_path, storage_module_info, storage_config).unwrap());

        // Pack the storage module
        storage_module.pack_with_zeros();

        let path = temporary_directory(None, false);
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        let database_provider = DatabaseProvider(Arc::new(db));

        let data_root = H256::random();

        let _ = storage_module.index_transaction_data(
            tx_path.to_vec(),
            data_root,
            LedgerChunkRange(ie(0, chunk_count as u64)),
        );

        for tx_chunk_offset in 0..chunk_count {
            let chunk = UnpackedChunk {
                data_root,
                data_size: chunk_size,
                data_path: data_path.to_vec().into(),
                bytes: chunk_data.to_vec().into(),
                tx_offset: tx_chunk_offset,
            };
            storage_module.write_data_chunk(&chunk).unwrap();
        }

        let _ = storage_module.sync_pending_chunks();

        let mining_broadcaster = BroadcastMiningService::new();
        let _mining_broadcaster_addr = mining_broadcaster.start();

        let vdf_service = VdfService::from_registry();
        let vdf_steps_guard: VdfStepsReadGuard =
            vdf_service.send(GetVdfStateMessage).await.unwrap();
        


        let partition_mining_actor = PartitionMiningActor::new(
            mining_address,
            database_provider.clone(),
            mocked_addr.0,
            vdf_steps_guard.into_inner_cloned(),
            storage_module,
            true,
            vdf_steps_guard.clone(),
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
            solution.chunk_offset < chunk_count * 2,
            "Not expected offset"
        );

        assert_eq!(
            mining_address, solution.mining_address,
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
}
