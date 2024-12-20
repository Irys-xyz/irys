use crate::block_producer::SolutionFoundMessage;
use crate::mining_broadcaster::{
    BroadcastDifficultyUpdate, BroadcastMiningSeed, MiningBroadcaster, Subscribe, Unsubscribe,
};
use actix::prelude::*;
use actix::{Actor, Addr, Context, Handler, Message};
use irys_storage::{ie, StorageModule};
use irys_types::app_state::DatabaseProvider;
use irys_types::{block_production::SolutionContext, H256, U256};
use irys_types::{Address, PartitionChunkOffset, SimpleRNG};
use openssl::sha;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub struct PartitionMiningActor {
    mining_address: Address,
    database_provider: DatabaseProvider,
    block_producer_actor: Recipient<SolutionFoundMessage>,
    mining_broadcaster_addr: Addr<MiningBroadcaster>,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
    difficulty: U256,
}

impl PartitionMiningActor {
    pub fn new(
        mining_address: Address,
        database_provider: DatabaseProvider,
        block_producer_addr: Recipient<SolutionFoundMessage>,
        mining_broadcaster_addr: Addr<MiningBroadcaster>,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
    ) -> Self {
        Self {
            mining_address,
            database_provider,
            block_producer_actor: block_producer_addr,
            mining_broadcaster_addr,
            storage_module,
            should_mine: start_mining,
            difficulty: U256::zero(),
        }
    }

    fn mine_partition_with_seed(
        &mut self,
        mining_seed: H256,
    ) -> eyre::Result<Option<SolutionContext>> {
        let partition_hash = match self.storage_module.partition_hash() {
            Some(p) => p,
            None => {
                info!("No parittion assigned !");
                return Ok(None);
            }
        };

        // Hash together the mining_seed and partition to get randomness for the rng
        let mut hasher = sha::Sha256::new();
        hasher.update(&mining_seed.0);
        hasher.update(&partition_hash.0);
        let rng_seed: u32 = u32::from_be_bytes(hasher.finish()[28..32].try_into().unwrap());
        let mut rng = SimpleRNG::new(rng_seed);

        let config = &self.storage_module.storage_config;

        // TODO: add a partition_state that keeps track of efficient sampling
        // For now, Pick a random recall range in the partition
        let num_chunks_in_partition = config.num_chunks_in_partition as u32;
        let num_chunks_in_recall_range = config.num_chunks_in_recall_range as u32;
        let recall_range_index =
            rng.next() % (num_chunks_in_partition / num_chunks_in_recall_range);

        // Starting chunk index within partition
        let start_chunk_offset = (recall_range_index * num_chunks_in_recall_range) as usize;

        // info!(
        //     "Recall range index {} start chunk index {}",
        //     recall_range_index, start_chunk_offset
        // );

        let read_range = ie(
            start_chunk_offset as u32,
            start_chunk_offset as u32 + config.num_chunks_in_recall_range as u32,
        );

        // haven't tested this, but it looks correct
        let chunks = self.storage_module.read_chunks(read_range)?;
        // debug!(
        //     "Got chunks {} from read range {:?}",
        //     &chunks.len(),
        //     &read_range
        // );

        if chunks.len() == 0 {
            warn!(
                "No chunks found - storage_module_id:{}",
                self.storage_module.id
            );
        }

        for (index, (chunk_offset, (chunk_bytes, _chunk_type))) in chunks.iter().enumerate() {
            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty
            let partition_chunk_offset = (start_chunk_offset + index) as PartitionChunkOffset;
            let (tx_path, data_path) = self
                .storage_module
                .read_tx_data_path(partition_chunk_offset as u64)?;

            // info!(
            //     "partition_hash: {}, chunk offset: {}",
            //     partition_hash, chunk_offset
            // );

            let mut hasher = sha::Sha256::new();
            hasher.update(chunk_bytes);
            hasher.update(&partition_chunk_offset.to_le_bytes());
            hasher.update(mining_seed.as_bytes());
            let test_solution = hash_to_number(&hasher.finish());

            if test_solution >= self.difficulty {
                //info!("SOLUTION FOUND!!!!!!!!!");

                //info!("{:?}", chunk_bytes);

                info!(
                    "Solution Found - partition_id: {}, ledger_offset: {}/{}, range_offset: {}/{}",
                    self.storage_module.id,
                    partition_chunk_offset,
                    num_chunks_in_partition,
                    index,
                    chunks.len()
                );

                let solution = SolutionContext {
                    partition_hash,
                    chunk_offset: partition_chunk_offset,
                    mining_address: self.mining_address,
                    tx_path, // capacity partitions have no tx_path nor data_path
                    data_path,
                    chunk: chunk_bytes.clone(),
                };

                // TODO: Let all partitions know to stop mining

                // Once solution is sent stop mining and let all other partitions know
                return Ok(Some(solution));
            } else {
                //  info!("NO SOLUTION!!!!")
            }
        }

        Ok(None)
    }
}

impl Actor for PartitionMiningActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        let broadcaster = &self.mining_broadcaster_addr;
        broadcaster.do_send(Subscribe(ctx.address()));
    }

    fn stopping(&mut self, ctx: &mut Context<Self>) -> Running {
        let broadcaster = &self.mining_broadcaster_addr;
        broadcaster.do_send(Unsubscribe(ctx.address()));
        Running::Stop
    }
}

#[derive(Debug, Clone)]
pub struct Seed(pub H256);

impl Seed {
    fn into_inner(self) -> H256 {
        self.0
    }
}

impl Handler<BroadcastMiningSeed> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMiningSeed, _: &mut Context<Self>) {
        let seed = msg.0;
        if !self.should_mine {
            debug!("Mining disabled, skipping seed {:?}", seed);
            return ();
        }

        debug!(
            "Partition {} -- looking for solution with difficulty >= {}",
            self.storage_module.partition_hash().unwrap(),
            self.difficulty
        );

        match self.mine_partition_with_seed(seed.into_inner()) {
            Ok(Some(s)) => match self.block_producer_actor.try_send(SolutionFoundMessage(s)) {
                Ok(_) => {
                    // debug!("Solution sent!");
                }
                Err(err) => error!("Error submitting solution to block producer {:?}", err),
            },

            Ok(None) => {
                //debug!("No solution sent!");
            }
            Err(err) => error!("Error in hanling mining solution {:?}", err),
        };
    }
}

impl Handler<BroadcastDifficultyUpdate> for PartitionMiningActor {
    type Result = ();

    fn handle(&mut self, msg: BroadcastDifficultyUpdate, _: &mut Context<Self>) {
        self.difficulty = msg.0.diff;
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
/// Message type for controlling mining
pub struct MiningControl(pub bool);

impl MiningControl {
    fn into_inner(self) -> bool {
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

fn hash_to_number(hash: &[u8]) -> U256 {
    U256::from_little_endian(hash)
}

#[cfg(test)]
mod tests {
    use crate::block_producer::{
        BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage,
    };
    use crate::mining::{PartitionMiningActor, Seed};
    use crate::mining_broadcaster::{BroadcastMiningSeed, MiningBroadcaster};
    use actix::{Actor, Addr, Recipient};
    use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::{
        ie, initialize_storage_files, read_info_file, StorageModule, StorageModuleInfo,
    };
    use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
    use irys_types::IrysBlockHeader;
    use irys_types::{
        app_state::DatabaseProvider, block_production::SolutionContext, chunk::UnpackedChunk,
        partition::PartitionAssignment, storage::LedgerChunkRange, Address, StorageConfig, H256,
    };
    use std::any::Any;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio::time::sleep;

    #[actix_rt::test]
    async fn test_solution() {
        let partition_hash = H256::random();
        let mining_address = Address::random();
        let chunks_number = 4;
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
            num_chunks_in_partition: chunks_number.try_into().unwrap(),
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: mining_address,
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1,
        };

        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: partition_hash,
                miner_address: mining_address,
                ledger_num: Some(0),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (ie(0, chunks_number), "hdd0".to_string()), // 0 to 3 inclusive, 4 chunks
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("storage_module_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let _ = initialize_storage_files(&base_path, &infos);

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
            LedgerChunkRange(ie(0, chunks_number as u64)),
        );

        for tx_chunk_offset in 0..chunks_number {
            let chunk = UnpackedChunk {
                data_root: data_root,
                data_size: chunk_size as u64,
                data_path: data_path.to_vec().into(),
                bytes: chunk_data.to_vec().into(),
                tx_offset: tx_chunk_offset,
            };
            storage_module.write_data_chunk(&chunk).unwrap();
        }

        let _ = storage_module.sync_pending_chunks();

        let mining_broadcaster = MiningBroadcaster::new();
        let mining_broadcaster_addr = mining_broadcaster.start();

        let partition_mining_actor = PartitionMiningActor::new(
            mining_address,
            database_provider.clone(),
            mocked_addr.0,
            mining_broadcaster_addr,
            storage_module,
            true,
        );

        let seed: Seed = Seed(H256::random());
        let _result = partition_mining_actor
            .start()
            .send(BroadcastMiningSeed(seed))
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
            solution.chunk_offset < chunks_number * 2,
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
