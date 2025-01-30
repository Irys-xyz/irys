use actix::Addr;
use irys_actors::{
    broadcast_mining_service::{BroadcastMiningSeed, BroadcastMiningService},
    vdf_service::{VdfSeed, VdfService},
};
use irys_types::{
    block_production::Seed,
    vdf_config::{AtomicVdfStepNumber, VDFStepsConfig},
    H256List, H256, U256,
};
use irys_vdf::{apply_reset_seed, step_number_to_salt_number, vdf_sha};
use sha2::{Digest, Sha256};
use std::sync::mpsc::Receiver;
use std::time::Instant;
use tracing::{debug, info};

pub fn run_vdf(
    config: VDFStepsConfig,
    global_step_number: u64,
    seed: H256,
    initial_reset_seed: H256,
    new_seed_listener: Receiver<H256>,
    shutdown_listener: Receiver<()>,
    broadcast_mining_service: Addr<BroadcastMiningService>,
    vdf_service: Addr<VdfService>,
    atomic_vdf_global_step: AtomicVdfStepNumber,
) {
    let mut hasher = Sha256::new();
    let mut hash: H256 = seed;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];
    let mut global_step_number = global_step_number;
    let mut reset_seed = initial_reset_seed;
    info!(
        "VDF thread started at global_step_number: {}",
        global_step_number
    );
    let nonce_limiter_reset_frequency = config.vdf_reset_frequency as u64;

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

        global_step_number += 1;
        atomic_vdf_global_step.store(global_step_number, std::sync::atomic::Ordering::Relaxed);

        let elapsed = now.elapsed();
        debug!("Vdf step duration: {:.2?}", elapsed);

        info!(
            "Seed created {} step number {}",
            hash.clone(),
            global_step_number
        );
        vdf_service.do_send(VdfSeed(Seed(hash)));
        broadcast_mining_service.do_send(BroadcastMiningSeed {
            seed: Seed(hash),
            checkpoints: H256List(checkpoints.clone()),
            global_step: global_step_number,
        });

        if global_step_number % nonce_limiter_reset_frequency == 0 {
            info!(
                "Reset seed {:?} applied to step {}",
                global_step_number, reset_seed
            );
            hash = apply_reset_seed(hash, reset_seed);
        }

        if shutdown_listener.try_recv().is_ok() {
            // Shutdown signal received
            break;
        };

        if let Ok(h) = new_seed_listener.try_recv() {
            debug!("New Send Seed {}", h); // TODO: wire new seed injections from chain accepted blocks message BlockConfirmedMessage
            reset_seed = h;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix::*;
    use irys_actors::vdf_service::{GetVdfStateMessage, VdfStepsReadGuard};
    use irys_types::*;
    use irys_vdf::{vdf_sha_verification, vdf_steps_are_valid};
    use nodit::interval::ii;
    use std::{
        sync::{atomic::AtomicU64, mpsc, Arc},
        time::Duration,
    };
    use tracing::{debug, level_filters::LevelFilter};
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt};

    fn init_tracing() {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();
    }
    #[actix_rt::test]
    async fn test_vdf_step() {
        let mut hasher = Sha256::new();
        let mut checkpoints: Vec<H256> = vec![H256::default(); CONFIG.num_checkpoints_in_vdf_step];
        let mut hash: H256 = H256::random();
        let original_hash = hash;
        let mut salt: U256 = U256::from(10);
        let original_salt = salt;

        init_tracing();

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

    #[actix_rt::test]
    async fn test_vdf_service() {
        let seed = H256::random();
        let reset_seed = H256::random();

        let vdf_config = VDFStepsConfig {
            vdf_reset_frequency: 2, // so to validation get into reset point
            vdf_difficulty: 1,      // go quicker
            ..VDFStepsConfig::default()
        };

        init_tracing();

        let broadcast_mining_service = BroadcastMiningService::from_registry();
        let vdf_service = VdfService::from_registry();
        let vdf_steps: VdfStepsReadGuard = vdf_service.send(GetVdfStateMessage).await.unwrap();

        let vdf_config2 = vdf_config.clone();
        let (_new_seed_tx, new_seed_rx) = mpsc::channel::<H256>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let atomic_global_step_number = Arc::new(AtomicU64::new(0));

        let vdf_thread_handler = std::thread::spawn(move || {
            run_vdf(
                vdf_config2,
                0,
                seed,
                reset_seed,
                new_seed_rx,
                shutdown_rx,
                broadcast_mining_service,
                vdf_service,
                atomic_global_step_number,
            )
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(10)).await;

        let step_num = vdf_steps.read().global_step;

        assert!(step_num > 4, "Should have more than 4 seeds");

        // get last 4 steps
        let steps = vdf_steps
            .read()
            .get_steps(ii(step_num - 3, step_num))
            .unwrap();

        // calculate last step checkpoints
        let mut hasher = Sha256::new();
        let mut salt = U256::from(step_number_to_salt_number(&vdf_config, step_num - 1_u64));
        let mut seed = steps[2];

        let mut checkpoints: Vec<H256> =
            vec![H256::default(); vdf_config.num_checkpoints_in_vdf_step];
        if step_num > 0 && (step_num - 1) % vdf_config.vdf_reset_frequency as u64 == 0 {
            seed = apply_reset_seed(seed, reset_seed);
        }
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            vdf_config.num_checkpoints_in_vdf_step,
            vdf_config.vdf_difficulty,
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
            vdf_steps_are_valid(&vdf_info, &vdf_config).is_ok(),
            "Invalid VDF"
        );

        // Send shutdown signal
        shutdown_tx.send(()).unwrap();

        // Wait for vdf thread to finish
        vdf_thread_handler.join().unwrap();
    }
}
