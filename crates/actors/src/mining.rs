use std::sync::{Arc, RwLock};

use crate::block_producer::{BlockProducerActor, SolutionFoundMessage};
use actix::{Actor, Addr, Context, Handler, Message};
use irys_storage::{ii, StorageModule};
use irys_types::app_state::DatabaseProvider;
use irys_types::Address;
use irys_types::{
    block_production::{Partition, SolutionContext},
    H256, NUM_CHUNKS_IN_RECALL_RANGE, NUM_RECALL_RANGES_IN_PARTITION, U256,
};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};
use tracing::{debug, error, info};

pub struct PartitionMiningActor {
    mining_address: Address,
    database_provider: DatabaseProvider,
    block_producer_actor: Addr<BlockProducerActor>,
    storage_module: Arc<StorageModule>,
    should_mine: bool,
}

impl PartitionMiningActor {
    pub fn new(
        mining_address: Address,
        database_provider: DatabaseProvider,
        block_producer_addr: Addr<BlockProducerActor>,
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

        // For now, Pick a random recall range in the partition
        let recall_range_index = rng.next_u64() % NUM_RECALL_RANGES_IN_PARTITION;

        // Starting chunk index within partition
        let start_chunk_index = (recall_range_index * NUM_CHUNKS_IN_RECALL_RANGE) as usize;

        // haven't tested this, but it looks correct
        let chunks;
        {
            chunks = self
                .storage_module
                .read_chunks(ii(
                    start_chunk_index as u32,
                    start_chunk_index as u32 + NUM_CHUNKS_IN_RECALL_RANGE as u32,
                ))
                .unwrap();
        }

        let mut hasher = Sha256::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let (_chunk_offset, chunk_data) = chunk;
            let (chunk_bytes, _chunk_type) = chunk_data;
            hasher.update(chunk_bytes);
            let hash = hasher.finalize_reset().to_vec();

            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty

            let solution_number = hash_to_number(&hash);
            if solution_number >= difficulty {
                dbg!("SOLUTION FOUND!!!!!!!!!");
                let solution = SolutionContext {
                    partition_hash: self.storage_module.partition_hash().unwrap(),
                    chunk_offset: (start_chunk_index + index) as u32,
                    mining_address: self.mining_address,
                };

                // TODO: Let all partitions know to stop mining

                // Once solution is sent stop mining and let all other partitions know
                return Some(solution);
            }
        }

        None
    }
}

impl Actor for PartitionMiningActor {
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

impl Handler<Seed> for PartitionMiningActor {
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
            Some(s) => match self.block_producer_actor.try_send(SolutionFoundMessage(s)) {
                Ok(_) => (),
                Err(err) => error!("Error submitting solution to block producer {:?}", err),
            },

            None => (),
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
