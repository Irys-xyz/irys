use actix::Addr;
use irys_actors::{
    broadcast_mining_service::{BroadcastMiningSeed, BroadcastMiningService},
    mining::Seed,
};
use irys_types::{vdf_config::VDFStepsConfig, H256, U256};
use openssl::sha;
use sha2::{Digest, Sha256};
use std::sync::mpsc::Receiver;
use std::time::Instant;
use tracing::debug;

/// Derives a salt value from the step_number for checkpoint hashing
///
/// # Arguments
///
/// * `step_number` - The step the checkpoint belongs to, add 1 to the salt for
/// each subsequent checkpoint calculation.
pub fn step_number_to_salt_number(config: &VDFStepsConfig, step_number: u64) -> u64 {
    match step_number {
        0 => 0,
        _ => (step_number - 1) * config.num_checkpoints_in_vdf_step as u64 + 1,
    }
}

/// Takes a checkpoint seed and applies the SHA256 block hash seed to it as
/// entropy. First it SHA256 hashes the `reset_seed` then SHA256 hashes the
/// output together with the `seed` hash.
///
/// # Arguments
///
/// * `seed` - The bytes of a SHA256 checkpoint hash
/// * `reset_seed` - The bytes of a SHA256 block hash used as entropy
///
/// # Returns
///
/// A new SHA256 seed hash containing the `reset_seed` entropy to use for
/// calculating checkpoints after the reset.
pub fn apply_reset_seed(seed: H256, reset_seed: H256) -> H256 {
    // Merge the current seed with the SHA256 has of the block hash.
    let mut hasher = sha::Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(reset_seed.as_bytes());
    H256::from(hasher.finish())
}

pub fn run_vdf(
    config: VDFStepsConfig,
    seed: H256,
    new_seed_listener: Receiver<H256>,
    broadcast_mining_service: Addr<BroadcastMiningService>,
) {
    let mut hasher = Sha256::new();
    let mut hash: H256 = H256::from_slice(seed.as_bytes());
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];
    let mut global_step_number: u64 = 0;
    let mut reset_seed = H256::random();
    // TODO: store vdf steps for efficient sampling here ?

    let nonce_limiter_reset_frequency = config.nonce_limiter_reset_frequency as u64;
    loop {
        let now = Instant::now();
        let mut salt = U256::from(step_number_to_salt_number(&config, global_step_number));

        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.vdf_difficulty,
            &mut checkpoints, // TODO: need to send also checkpoints to block producer for last_step_checkpoints ?
        );

        let elapsed = now.elapsed();
        debug!("Vdf step duration: {:.2?}", elapsed);

        debug!("Seed created {}", hash.clone());
        broadcast_mining_service.do_send(BroadcastMiningSeed(Seed(hash)));

        if let Ok(h) = new_seed_listener.try_recv() {
            debug!("New Send Seed {}", h); // TODO: wire new seed injections from chain accepted blocks message BlockConfirmedMessage
            reset_seed = h;
        }

        global_step_number += 1;
        if global_step_number % nonce_limiter_reset_frequency == 0 {
            hash = apply_reset_seed(hash, reset_seed);
        }
    }
}

#[inline]
pub fn vdf_sha(
    hasher: &mut Sha256,
    salt: &mut U256,
    seed: &mut H256,
    num_checkpoints: usize,
    num_iterations: u64,
    checkpoints: &mut Vec<H256>,
) {
    let mut local_salt: [u8; 32] = [0; 32];

    for checkpoint_idx in 0..num_checkpoints {
        salt.to_little_endian(&mut local_salt);

        for _ in 0..num_iterations {
            hasher.update(local_salt);
            hasher.update(seed.as_bytes());
            *seed = H256(hasher.finalize_reset().into());
        }

        // Store the result at the correct checkpoint index
        checkpoints[checkpoint_idx] = *seed;

        // Increment the salt for the next checkpoint calculation
        *salt = *salt + 1;
    }
}

/// Vdf verification code
pub fn vdf_sha_verification(
    salt: U256,
    seed: H256,
    num_checkpoints: usize,
    num_iterations: usize,
) -> Vec<H256> {
    let mut local_salt: U256 = salt;
    let mut local_seed: H256 = seed;
    let mut salt_bytes: H256 = H256::zero();
    let mut checkpoints: Vec<H256> = vec![H256::default(); num_checkpoints];

    for checkpoint_idx in 0..num_checkpoints {
        //  initial checkpoint hash
        // -----------------------------------------------------------------
        if checkpoint_idx != 0 {
            // If the index is > 0, use the previous checkpoint as the seed
            local_seed = checkpoints[checkpoint_idx - 1];
        }

        local_salt.to_little_endian(salt_bytes.as_mut());

        // Hash salt+seed
        let mut hasher = sha::Sha256::new();
        hasher.update(salt_bytes.as_bytes());
        hasher.update(local_seed.as_bytes());
        let mut hash_bytes = H256::from(hasher.finish());

        // subsequent hash iterations (if needed)
        // -----------------------------------------------------------------
        for _ in 1..num_iterations {
            let mut hasher = sha::Sha256::new();
            hasher.update(salt_bytes.as_bytes());
            hasher.update(hash_bytes.as_bytes());
            hash_bytes = H256::from(hasher.finish());
        }

        // Store the result at the correct checkpoint index
        checkpoints[checkpoint_idx] = hash_bytes;

        // Increment the salt for the next checkpoint calculation
        local_salt = local_salt + 1;
    }
    checkpoints
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::NUM_CHECKPOINTS_IN_VDF_STEP;
    use tracing::{debug, level_filters::LevelFilter};
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt};

    #[actix_rt::test]
    async fn test_vdf() {
        let mut hasher = Sha256::new();
        let mut checkpoints: Vec<H256> = vec![H256::default(); NUM_CHECKPOINTS_IN_VDF_STEP];
        let mut hash: H256 = H256::random();
        let original_hash = hash;
        let mut salt: U256 = U256::from(10);
        let original_salt = salt;

        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();

        let config = VDFStepsConfig::default();

        debug!("VDF difficulty: {}", config.vdf_difficulty);
        let now = Instant::now();
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.vdf_difficulty,
            &mut checkpoints,
        );
        let elapsed = now.elapsed();
        debug!("vdf step: {:.2?}", elapsed);

        let now = Instant::now();
        let checkpoints2 = vdf_sha_verification(
            original_salt,
            original_hash,
            config.num_checkpoints_in_vdf_step,
            config.vdf_difficulty as usize,
        );
        let elapsed = now.elapsed();
        debug!("vdf original code verification: {:.2?}", elapsed);

        assert_eq!(checkpoints, checkpoints2, "Should be equal");
    }

    // TODO: not important test, but check if is doable
    // #[actix_rt::test]
    // async fn test_vdf_actor() {
    //     use actix::actors::mocker::Mocker;
    //     use irys_actors::mining::PartitionMiningActor;
    //     use irys_actors::mining_broadcaster::Subscribe;
    //     use std::sync::mpsc;
    //     use tokio::time::{sleep, Duration};
    //     type PartitionMockMiner = Mocker<PartitionMiningActor>;

    //     let _ = SubscriberBuilder::default()
    //         .with_max_level(LevelFilter::DEBUG)
    //         .finish()
    //         .try_init();

    //     let seed = H256::random();
    //     let next_seed = H256::random();

    //     let (miner_seed_tx, miner_seed_rx) = mpsc::channel::<Seed>();

    //     let mining_broadcaster = MiningBroadcaster::new();
    //     let mining_broadcaster_addr = mining_broadcaster.start();

    //     let mocked_miner = PartitionMockMiner::mock(Box::new(move |msg, _ctx| {
    //         let rcv_seed = *msg.downcast::<Seed>().unwrap();
    //         debug!("seed received by miner {:?}", rcv_seed);
    //         miner_seed_tx.send(rcv_seed).unwrap();
    //         Box::new(Some(()))
    //     }));

    //     let mining_addr: Addr<PartitionMockMiner> = mocked_miner.start();

    //     // TODO: check if next error could be overcome
    //     // mining_broadcaster_addr.do_send(Subscribe(mining_addr.recipient()));
    //     let (new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();

    //     new_seed_tx.send(next_seed).unwrap();

    //     std::thread::spawn(move || {
    //         run_vdf(
    //             VDFStepsConfig::default(),
    //             seed,
    //             new_seed_rx,
    //             mining_broadcaster_addr.clone(),
    //         )
    //     });

    //     //wait for seed to arrive
    //     sleep(Duration::from_millis(1200)).await;

    //     let _ = miner_seed_rx.recv().unwrap();
    // }
}
