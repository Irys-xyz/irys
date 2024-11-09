use std::sync::Arc;

use crate::{block_producer::BlockProducerActor, chunk_storage::ChunkStorageActor};
use actix::{Actor, Addr, Context, Handler, Message};
use irys_storage::StorageProvider;
use irys_storage::{ie, partition_provider::PartitionStorageProvider};
use irys_types::app_state::DatabaseProvider;
use irys_types::{
    block_production::{Partition, SolutionContext},
    Ranges, CHUNK_SIZE, H256, NUM_CHUNKS_IN_RECALL_RANGE, NUM_RECALL_RANGES_IN_PARTITION, U256,
};
use sha2::{Digest, Sha256};

pub struct PartitionMiningActor {
    partition: Partition,
    database_provider: DatabaseProvider,
    block_producer_actor: Addr<BlockProducerActor>,
    part_storage_provider: PartitionStorageProvider,
    ranges: Ranges,
}

impl PartitionMiningActor {
    pub fn new(
        partition: Partition,
        database_provider: DatabaseProvider,
        block_producer_addr: Addr<BlockProducerActor>,
        storage_provider: PartitionStorageProvider,
    ) -> Self {
        Self {
            partition,
            database_provider,
            block_producer_actor: block_producer_addr,
            part_storage_provider: storage_provider,
            ranges: Ranges::new(),
        }
    }

    fn mine_partition_with_seed(
        &mut self,
        seed: H256,
        difficulty: U256,
    ) -> Option<SolutionContext> {
        // efficient sampling
        let recall_range_index = self.ranges.next_recall_range(seed);

        // Starting chunk index within partition
        let start_chunk_index = recall_range_index * NUM_CHUNKS_IN_RECALL_RANGE as u32;

        // haven't tested this, but it looks correct
        let chunks = self
            .part_storage_provider
            .read_chunks(
                ie(
                    start_chunk_index,
                    start_chunk_index + NUM_CHUNKS_IN_RECALL_RANGE as u32,
                ),
                None,
            )
            .unwrap();

        let mut hasher = Sha256::new();
        for (index, chunk) in chunks.iter().enumerate() {
            hasher.update(chunk);
            let hash = hasher.finalize_reset().to_vec();

            // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty

            let solution_number = hash_to_number(&hash);
            if solution_number >= difficulty {
                dbg!("SOLUTION FOUND!!!!!!!!!");
                let solution = SolutionContext {
                    partition_id: self.partition.id,
                    chunk_index: start_chunk_index + index as u32,
                    mining_address: self.partition.mining_address,
                };
                // TODO: Send info to block builder code

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

#[derive(Message)]
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
        let difficuly = get_latest_difficulty(&self.database_provider);
        match self.mine_partition_with_seed(seed.into_inner(), difficuly) {
            Some(s) => {
                let _ = self.block_producer_actor.send(s);
            }
            None => (),
        };
    }
}

fn get_latest_difficulty(db: &DatabaseProvider) -> U256 {
    U256::zero()
}

fn hash_to_number(hash: &[u8]) -> U256 {
    U256::from_little_endian(hash)
}
