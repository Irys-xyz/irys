use actix::Addr;
use irys_actors::mining::{PartitionMiningActor, Seed};
use irys_types::{H256, HASHES_PER_CHECKPOINT, NUM_CHECKPOINTS_IN_VDF_STEP, VDF_SHA_1S};
use sha2::{Digest, Sha256};
use std::sync::mpsc::Receiver;
use tracing::debug;

pub fn run_vdf(
    seed: H256,
    new_seed_listener: Receiver<H256>,
    partition_channels: Vec<Addr<PartitionMiningActor>>,
) {
    let mut hasher = Sha256::new();

    let mut hash: H256 = H256::from_slice(seed.as_bytes());

    loop {
        let mut checkpoints: Vec<H256> = Vec::with_capacity(NUM_CHECKPOINTS_IN_VDF_STEP);
        for i in 0..VDF_SHA_1S {
            hasher.update(hash);
            let hash_result = hasher.finalize_reset().to_vec();
            hash = H256::from_slice(&hash_result);
            if (i + 1) % HASHES_PER_CHECKPOINT == 0 {
                // write checkpoint
                checkpoints.push(hash);
            }
        }

        for a in &partition_channels {
            debug!("Seed created {}", hash.clone());
            a.do_send(Seed(hash));
        }

        if let Ok(h) = new_seed_listener.try_recv() {
            hash = h;
        }
    }
}
