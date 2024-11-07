use std::sync::Arc;

use actix::{Actor, Addr, Context, Handler, Message};
use irys_storage::{ie, partition_provider::PartitionStorageProvider};
use irys_types::{
    block_production::{Partition, SolutionContext},
    CHUNK_SIZE, H256, NUM_CHUNKS_IN_RECALL_RANGE, NUM_RECALL_RANGES_IN_PARTITION, U256,
};
use rand::{seq::SliceRandom, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

use crate::{
    block_producer::BlockProducerActor, chunk_storage::ChunkStorageActor,
    storage_provider::StorageProvider,
};

pub struct PartitionMiningActor {
    partition: Partition,
    block_producer_actor: Addr<BlockProducerActor>,
    part_storage_provider: PartitionStorageProvider,
}

impl PartitionMiningActor {
    pub fn new(
        partition: Partition,
        block_producer_addr: Addr<BlockProducerActor>,
        storage_provider: PartitionStorageProvider,
    ) -> Self {
        Self {
            partition,
            block_producer_actor: block_producer_addr,
            part_storage_provider: storage_provider,
        }
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
        let difficuly = get_latest_difficulty();
        match mine_partition_with_seed(
            &self.partition,
            &self.part_storage_provider,
            seed.into_inner(),
            difficuly,
        ) {
            Some(s) => {
                let _ = self.block_producer_actor.send(s);
            }
            None => (),
        };
    }
}

fn mine_partition_with_seed(
    partition: &Partition,
    storage_provider: &PartitionStorageProvider,
    seed: H256,
    difficulty: U256,
) -> Option<SolutionContext> {
    // TODO: add a partition_state that keeps track of efficient sampling
    let mut rng = ChaCha20Rng::from_seed(seed.into());

    // For now, Pick a random recall range in the partition
    let recall_range_index = rng.next_u64() % NUM_RECALL_RANGES_IN_PARTITION;

    // Starting chunk index within partition
    let start_chunk_index = (recall_range_index * NUM_CHUNKS_IN_RECALL_RANGE) as usize;

    // Create a contiguous piece of memory on the heap where chunks can be written into
    let mut chunks_buffer: Vec<[u8; CHUNK_SIZE as usize]> =
        Vec::with_capacity((NUM_CHUNKS_IN_RECALL_RANGE * CHUNK_SIZE) as usize);

    // // TODO: read chunks. For now creates random
    // for _ in 0..NUM_CHUNKS_IN_RECALL_RANGE {
    //     let mut data = [0u8; CHUNK_SIZE as usize];
    //     rand::thread_rng().fill_bytes(&mut data);
    //     chunks_buffer.push(data);
    // }

    // haven't tested this, but it looks correct :p
    let chunks = storage_provider.read_chunks(ie(0, NUM_CHUNKS_IN_RECALL_RANGE as u32), None);

    let mut hasher = Sha256::new();
    for i in 0..NUM_CHUNKS_IN_RECALL_RANGE {
        let chunk: &[u8] = &chunks_buffer[i as usize];

        hasher.update(chunk);
        let hash = hasher.finalize_reset().to_vec();

        // TODO: check if difficulty higher now. Will look in DB for latest difficulty info and update difficulty

        let solution_number = hash_to_number(&hash);
        if solution_number >= difficulty {
            dbg!("SOLUTION FOUND!!!!!!!!!");
            let solution = SolutionContext {
                partition_id: partition.id,
                // TODO: Fix
                chunk_index: 0,
                mining_address: partition.mining_addr,
            };
            // TODO: Send info to block builder code

            // TODO: Let all partitions know to stop mining

            // Once solution is sent stop mining and let all other partitions know
            return Some(solution);
        }
    }

    None
}

fn get_latest_difficulty() -> U256 {
    U256::max_value()
}

fn hash_to_number(hash: &[u8]) -> U256 {
    U256::from_little_endian(hash)
}
