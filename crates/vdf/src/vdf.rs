use crate::state::AtomicVdfState;
use crate::{apply_reset_seed, step_number_to_salt_number, vdf_sha, MiningBroadcaster, VdfStep};
use irys_types::{
    block_production::Seed, AtomicVdfStepNumber, H256List, IrysBlockHeader, H256, U256,
};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use tracing::{debug, info};

pub fn run_vdf_for_genesis_block(
    genesis_block: &mut IrysBlockHeader,
    config: &irys_types::VdfConfig,
) {
    let last_epoch_block_hash = genesis_block.last_epoch_hash;
    genesis_block.vdf_limiter_info.prev_output = last_epoch_block_hash;

    let mut hash: H256 = genesis_block.vdf_limiter_info.seed;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];

    for global_step_number in 0..=1 {
        let mut hasher = Sha256::new();
        let mut salt = U256::from(step_number_to_salt_number(config, global_step_number));

        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.sha_1s_difficulty,
            &mut checkpoints,
        );

        if global_step_number == 0 {
            genesis_block.vdf_limiter_info.prev_output = hash;
        } else {
            genesis_block.vdf_limiter_info.global_step_number = 1;
            genesis_block.vdf_limiter_info.output = hash;
            genesis_block.vdf_limiter_info.last_step_checkpoints.0 = checkpoints.clone();
            genesis_block.vdf_limiter_info.steps.0 = vec![hash];
        }
    }
}

pub fn run_vdf(
    config: &irys_types::VdfConfig,
    global_step_number: u64,
    seed: H256,
    initial_reset_seed: H256,
    mut fast_forward_receiver: Receiver<VdfStep>,
    mut vdf_mining_state_listener: Receiver<bool>,
    mut shutdown_listener: Receiver<()>,
    broadcast_mining_service: impl MiningBroadcaster,
    vdf_state: AtomicVdfState,
    atomic_vdf_global_step: AtomicVdfStepNumber,
) {
    let mut hasher = Sha256::new();
    let mut hash: H256 = seed;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];
    let mut global_step_number = global_step_number;
    // FIXME: The reset seed is the same as the seed... which I suspect is incorrect!
    let reset_seed = initial_reset_seed;
    info!(
        "VDF thread started at global_step_number: {}",
        global_step_number
    );
    let nonce_limiter_reset_frequency = config.reset_frequency as u64;

    // maintain a state of whether or not this vdf loop should be mining
    let mut vdf_mining: bool = true;

    loop {
        if shutdown_listener.try_recv().is_ok() {
            tracing::info!("VDF loop shutdown signal received");
            break;
        };

        // check for VDF fast forward step
        while let Ok(proposed_ff_step) = fast_forward_receiver.try_recv() {
            // if the step number is ahead of local nodes vdf steps
            if global_step_number < proposed_ff_step.global_step_number {
                debug!(
                    "Fastforward Step {:?} with Seed {:?}",
                    proposed_ff_step.global_step_number, proposed_ff_step.step
                );
                hash = proposed_ff_step.step;
                global_step_number = store_step(
                    hash,
                    &atomic_vdf_global_step,
                    &vdf_state,
                    proposed_ff_step.global_step_number,
                );
                hash = process_reset(
                    global_step_number,
                    nonce_limiter_reset_frequency,
                    hash,
                    reset_seed,
                );
            } else {
                debug!(
                    "Fastforward Step {} is not ahead of {}",
                    proposed_ff_step.global_step_number, global_step_number
                );
            }
        }

        // check if vdf mining state should change
        if let Ok(new_mining_state) = vdf_mining_state_listener.try_recv() {
            tracing::info!("Setting mining state to {}", new_mining_state);
            vdf_mining = new_mining_state;
        }

        // if mining disabled, wait 200ms and continue loop i.e. check again
        if !vdf_mining {
            tracing::trace!("VDF Mining Paused, waiting 200ms");
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        let now = Instant::now();

        let mut salt = U256::from(step_number_to_salt_number(config, global_step_number));

        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.sha_1s_difficulty,
            &mut checkpoints, // TODO: need to send also checkpoints to block producer for last_step_checkpoints?
        );

        let elapsed = now.elapsed();
        debug!("Vdf step duration: {:.2?}", elapsed);

        global_step_number = store_step(
            hash,
            &atomic_vdf_global_step,
            &vdf_state,
            global_step_number + 1,
        );
        info!(
            "Seed created {} step number {}",
            hash.clone(),
            global_step_number
        );

        broadcast_mining_service.broadcast(
            Seed(hash),
            H256List(checkpoints.clone()),
            global_step_number,
        );

        hash = process_reset(
            global_step_number,
            nonce_limiter_reset_frequency,
            hash,
            reset_seed,
        );
    }
    debug!(?global_step_number, "VDF thread stopped");
}

#[must_use]
fn process_reset(
    global_step_number: u64,
    nonce_limiter_reset_frequency: u64,
    hash: H256,
    reset_seed: H256,
) -> H256 {
    if global_step_number % nonce_limiter_reset_frequency == 0 {
        // FIXME: is there an issue with reset_seed never changing here?
        info!(
            "Reset seed {:?} applied to step {}",
            global_step_number, reset_seed
        );
        apply_reset_seed(hash, reset_seed)
    } else {
        hash
    }
}

#[must_use]
fn store_step(
    hash: H256,
    atomic_vdf_global_step: &AtomicVdfStepNumber,
    vdf_state: &AtomicVdfState,
    new_global_step_number: u64,
) -> u64 {
    let global_step_number = {
        vdf_state
            .write()
            .expect("to write to VDF")
            .store_step(Seed(hash), new_global_step_number)
    };
    atomic_vdf_global_step.store(global_step_number, std::sync::atomic::Ordering::Relaxed);
    global_step_number
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::mocked_vdf_service;
    use crate::state::{vdf_steps_are_valid, VdfStateReadonly};
    use crate::vdf_sha_verification;
    use irys_types::*;
    use nodit::interval::ii;
    use std::{
        sync::{atomic::AtomicU64, Arc},
        time::Duration,
    };
    use tokio::sync::mpsc;
    use tracing::{debug, level_filters::LevelFilter};
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt};

    struct MockMining;

    impl MiningBroadcaster for MockMining {
        fn broadcast(&self, _seed: Seed, _checkpoints: H256List, _global_step: u64) {}
    }

    fn init_tracing() {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();
    }

    #[actix_rt::test]
    async fn test_vdf_step() {
        let config = Config::new(NodeConfig::testnet());
        let mut hasher = Sha256::new();
        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.consensus.vdf.num_checkpoints_in_vdf_step];
        let mut hash: H256 = H256::random();
        let original_hash = hash;
        let mut salt: U256 = U256::from(10);
        let original_salt = salt;

        init_tracing();

        debug!("VDF difficulty: {}", config.consensus.vdf.sha_1s_difficulty);
        let now = Instant::now();
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut hash,
            config.consensus.vdf.num_checkpoints_in_vdf_step,
            config.consensus.vdf.sha_1s_difficulty,
            &mut checkpoints,
        );
        let elapsed = now.elapsed();
        debug!("vdf step: {:.2?}", elapsed);

        let now = Instant::now();
        let checkpoints2 = vdf_sha_verification(
            original_salt,
            original_hash,
            config.consensus.vdf.num_checkpoints_in_vdf_step,
            config.consensus.vdf.sha_1s_difficulty as usize,
        );
        let elapsed = now.elapsed();
        debug!("vdf original code verification: {:.2?}", elapsed);

        assert_eq!(checkpoints, checkpoints2, "Should be equal");
    }

    #[actix_rt::test]
    async fn test_vdf_service() {
        let mut node_config = NodeConfig::testnet();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new(node_config);

        let seed = H256::random();
        let reset_seed = H256::random();

        init_tracing();

        let broadcast_mining_service = MockMining;
        let (_, ff_step_receiver) = mpsc::channel::<VdfStep>(1);
        let (_, mining_state_rx) = mpsc::channel::<bool>(1);

        let vdf_state = mocked_vdf_service(&config).await;
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let atomic_global_step_number = Arc::new(AtomicU64::new(0));

        let vdf_thread_handler = std::thread::spawn({
            let config = config.clone();
            move || {
                run_vdf(
                    &config.consensus.vdf,
                    0,
                    seed,
                    reset_seed,
                    ff_step_receiver,
                    mining_state_rx,
                    shutdown_rx,
                    broadcast_mining_service,
                    vdf_state.clone(),
                    atomic_global_step_number,
                )
            }
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(10)).await;

        let step_num = vdf_steps_guard.read().global_step;

        assert!(
            step_num > 4,
            "Should have more than 4 seeds, only have {}",
            step_num
        );

        // get last 4 steps
        let steps = vdf_steps_guard
            .read()
            .get_steps(ii(step_num - 3, step_num))
            .unwrap();

        // calculate last step checkpoints
        let mut hasher = Sha256::new();
        let mut salt = U256::from(step_number_to_salt_number(
            &config.consensus.vdf,
            step_num - 1_u64,
        ));
        let mut seed = steps[2];

        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.consensus.vdf.num_checkpoints_in_vdf_step];
        if step_num > 0 && (step_num - 1) % config.consensus.vdf.reset_frequency as u64 == 0 {
            seed = apply_reset_seed(seed, reset_seed);
        }
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            config.consensus.vdf.num_checkpoints_in_vdf_step,
            config.consensus.vdf.sha_1s_difficulty,
            &mut checkpoints,
        );

        let vdf_info = VDFLimiterInfo {
            global_step_number: step_num,
            output: steps[3],
            prev_output: steps[0],
            steps: H256List(steps.0[1..=3].into()),
            last_step_checkpoints: H256List(checkpoints),
            seed: reset_seed,
            ..VDFLimiterInfo::default()
        };

        assert!(
            vdf_steps_are_valid(&vdf_info, &config.consensus.vdf, vdf_steps_guard).is_ok(),
            "Invalid VDF"
        );

        // Send shutdown signal
        shutdown_tx.send(()).await.unwrap();

        // Wait for vdf thread to finish
        vdf_thread_handler.join().unwrap();
    }
}
