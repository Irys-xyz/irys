use std::sync::{Arc, RwLock};

use crate::block_producer::BlockProducerActor;
use actix::{dev::ToEnvelope, Actor, Addr, Context, Handler, Message};
use irys_storage::{ie, StorageModule};
use irys_types::app_state::DatabaseProvider;
use irys_types::Address;
use irys_types::{block_production::SolutionContext, H256, U256};
use openssl::sha;
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use tracing::{debug, error};

pub struct PartitionMiningActor<T = BlockProducerActor>
where
    T: Actor,
{
    mining_address: Address,
    database_provider: DatabaseProvider,
    block_producer_actor: Addr<T>,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
}

impl<T> PartitionMiningActor<T>
where
    T: Actor,
{
    pub fn new(
        mining_address: Address,
        database_provider: DatabaseProvider,
        block_producer_addr: Addr<T>,
        storage_module: Arc<StorageModule>,
        start_mining: bool,
    ) -> Self {
        Self {
            mining_address,
            database_provider,
            block_producer_actor: block_producer_addr,
            storage_module,
            should_mine: start_mining,
        }
    }

    fn mine_partition_with_seed(
        &mut self,
        seed: H256,
        difficulty: U256,
    ) -> Option<SolutionContext> {
        // TODO: add a partition_state that keeps track of efficient sampling
        let mut rng = ChaCha20Rng::from_seed(seed.into());

        let config = self.storage_module.config.clone();

        // For now, Pick a random recall range in the partition
        let recall_range_index =
            rng.next_u64() % (config.num_chunks_in_partition / config.num_chunks_in_recall_range);

        // Starting chunk index within partition
        let start_chunk_index = (recall_range_index * config.num_chunks_in_recall_range) as usize;

        debug!(
            "Recall range index {} start chunk index {}",
            recall_range_index, start_chunk_index
        );

        // haven't tested this, but it looks correct
        let chunks = self
            .storage_module
            .read_chunks(ie(
                start_chunk_index as u32,
                start_chunk_index as u32 + config.num_chunks_in_recall_range as u32,
            ))
            .unwrap();

        for (index, chunk) in chunks.iter().enumerate() {
            let (_chunk_offset, (chunk_bytes, _chunk_type)) = chunk;
            let hash = sha::sha256(chunk_bytes);

            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty

            let solution_number = hash_to_number(&hash);
            let solution_chunk_offset = (start_chunk_index + index) as u32;
            let (tx_path, data_path) = self
                .storage_module
                .read_tx_data_path(solution_chunk_offset as u64)
                .unwrap();
            if solution_number >= difficulty {
                debug!("SOLUTION FOUND!!!!!!!!!");
                let solution = SolutionContext {
                    partition_hash: self.storage_module.partition_hash().unwrap(),
                    chunk_offset: solution_chunk_offset,
                    mining_address: self.mining_address,
                    tx_path: tx_path.unwrap(),
                    data_path: data_path.unwrap(),
                    chunk: chunk_bytes.clone(),
                };

                // TODO: Let all partitions know to stop mining

                // Once solution is sent stop mining and let all other partitions know
                return Some(solution);
            }
        }

        None
    }
}

impl<T> Actor for PartitionMiningActor<T>
where
    T: Actor,
{
    type Context = Context<Self>;
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct Seed(pub H256);

impl Seed {
    fn into_inner(self) -> H256 {
        self.0
    }
}

impl<T> Handler<Seed> for PartitionMiningActor<T>
where
    T: Actor<Context = Context<T>> + Handler<SolutionContext>,
{
    type Result = ();

    fn handle(&mut self, seed: Seed, _ctx: &mut Context<Self>) -> Self::Result {
        if !self.should_mine {
            debug!("Mining disabled, skipping seed {:?}", seed);
            return ();
        }

        let difficulty = get_latest_difficulty(&self.database_provider);

        debug!(
            "Partition {} -- looking for solution with difficulty >= {}",
            self.storage_module.partition_hash().unwrap(),
            difficulty
        );

        match self.mine_partition_with_seed(seed.into_inner(), difficulty) {
            Some(s) => match self.block_producer_actor.try_send(s) {
                Ok(_) => {
                    debug!("Solution sent!");
                    ()
                }
                Err(err) => error!("Error submitting solution to block producer {:?}", err),
            },

            None => {
                debug!("No solution sent!");
                ()
            }
        };
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

fn get_latest_difficulty(db: &DatabaseProvider) -> U256 {
    U256::zero()
}

fn hash_to_number(hash: &[u8]) -> U256 {
    U256::from_little_endian(hash)
}

#[cfg(test)]
mod tests {
    use crate::mining::{BlockProducerActor, PartitionMiningActor, Seed};
    //use actix::SystemRegistry;
    use actix::actors::mocker::Mocker;
    type BlockProducerMockActor = Mocker<BlockProducerActor>;

    use actix::{prelude::*, Actor, Addr};
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::{
        ie, initialize_storage_files, read_info_file, StorageModule, StorageModuleInfo,
    };
    use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
    use irys_types::{
        app_state::DatabaseProvider, block_production::SolutionContext, chunk::Chunk,
        partition::PartitionAssignment, storage::LedgerChunkRange, Address, StorageConfig, H256,
    };
    use std::sync::Arc;
    use tracing::{debug, error, info};

    #[actix_rt::test]
    async fn test_solution() {
        let partition_hash = H256::random();
        let mining_address = Address::random();
        let chunks_number = 4;
        let chunk_size = 32;
        let chunk_data = [0; 32];
        let data_path = [4, 3, 2, 1];
        let tx_path = [4, 3, 2, 1];

        let mocked_block_producer = BlockProducerMockActor::mock(Box::new(move |msg, _ctx| {
            let solution: SolutionContext = *msg.downcast::<SolutionContext>().unwrap();
            assert_eq!(
                partition_hash, solution.partition_hash,
                "Not expected partition"
            );
            assert!(
                solution.chunk_offset < chunks_number * 2,
                "Not expected oftset"
            );
            assert_eq!(
                mining_address, solution.mining_address,
                "Not expected partition"
            );
            assert_eq!(tx_path.to_vec(), solution.tx_path, "Not expected partition");
            assert_eq!(
                data_path.to_vec(),
                solution.data_path,
                "Not expected partition"
            );
            Box::new(())
        }));

        let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
        //SystemRegistry::set(block_producer_actor_addr);

        // Set up the storage geometry for this test
        let storage_config = StorageConfig {
            chunk_size,
            num_chunks_in_partition: 4,
            num_chunks_in_recall_range: 2,
            num_partitions_in_slot: 1,
            miner_address: mining_address,
            min_writes_before_sync: 1,
            entropy_packing_iterations: 1,
        };

        let infos = vec![StorageModuleInfo {
            module_num: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: partition_hash,
                miner_address: mining_address,
                ledger_num: Some(1),
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
        let mut storage_module = Arc::new(StorageModule::new(
            &base_path,
            storage_module_info,
            Some(storage_config),
        ));

        let path = temporary_directory(None, false);
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        let database_provider = DatabaseProvider(Arc::new(db));

        let data_root = H256::random();

        for i in 0..chunks_number {
            let chunk = Chunk {
                data_root: H256::zero(),
                data_size: chunk_size as u64,
                data_path: data_path.to_vec().into(),
                bytes: chunk_data.to_vec().into(),
                offset: i,
            };
            storage_module.write_data_chunk(chunk, i as u64).unwrap();
        }
        let _ = storage_module.index_transaction_data(
            tx_path.to_vec(),
            data_root,
            LedgerChunkRange(ie(0, chunks_number as u64)),
        );
        let _ = storage_module.sync_pending_chunks();

        let partition_mining_actor = PartitionMiningActor::new(
            mining_address,
            database_provider.clone(),
            block_producer_actor_addr.clone(),
            storage_module,
            true,
        );

        let seed: Seed = Seed(H256::random());
        let _result = partition_mining_actor.start().send(seed).await.unwrap();
    }
}
