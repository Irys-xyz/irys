//! This crate provides functions and utilities for VDF (Verifiable Delay Function) operations,
//! including checkpoint validation and seed application.

use irys_types::{H256List, VDFLimiterInfo, VDFStepsConfig, H256, U256};

use openssl::sha;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{error, info};

/// Derives a salt value from the `step_number` for checkpoint hashing
///
/// # Arguments
///
/// * `step_number` - The step the checkpoint belongs to, add 1 to the salt for
/// each subsequent checkpoint calculation.
pub const fn step_number_to_salt_number(config: &VDFStepsConfig, step_number: u64) -> u64 {
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

/// Validate the checkpoints from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
///
/// # Arguments
///
/// * `vdf_info` - The Vdf limiter info from the block header to validate.
///
/// # Returns
///
/// - `bool` - `true` if the checkpoints are valid, false otherwise.
pub fn checkpoints_are_valid(
    vdf_info: &VDFLimiterInfo,
    config: &VDFStepsConfig,
) -> eyre::Result<()> {
    info!(
        "Checking seed {:?} reset_seed {:?}",
        vdf_info.prev_output, vdf_info.seed
    );

    let reset_seed = vdf_info.seed;

    let mut step_hashes = vdf_info.checkpoints.clone();

    // Add the seed from the previous nonce info to the steps
    let previous_seed = vdf_info.prev_output;
    step_hashes.0.insert(0, previous_seed);

    // Make a read only copy for parallel iterating
    let steps = step_hashes.clone();

    // Calculate the step number of the first step in the blocks sequence
    let start_step_number: u64 = vdf_info.global_step_number - vdf_info.checkpoints.len() as u64;

    // We must calculate the checkpoint iterations for each step sequentially
    // because we only have the first and last checkpoint of each step, but we
    // can calculate each of the steps in parallel
    // Limit threads number to avoid overloading the system using configuration limit
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.vdf_parallel_verification_thread_limit)
        .build()
        .unwrap();
    let test: Vec<(H256, Option<H256List>)> = pool.install(|| {
        (0..steps.len() - 1)
            .into_par_iter()
            .map(|i| {
                let mut hasher = Sha256::new();
                let mut salt = U256::from(step_number_to_salt_number(
                    config,
                    start_step_number + i as u64,
                ));
                let mut seed = steps[i];
                let mut checkpoints: Vec<H256> =
                    vec![H256::default(); config.num_checkpoints_in_vdf_step];
                if start_step_number + i as u64 > 0
                    && (start_step_number + i as u64) % config.nonce_limiter_reset_frequency as u64
                        == 0
                {
                    info!(
                        "Applying reset seed {:?} to step number {}",
                        reset_seed,
                        start_step_number + i as u64
                    );
                    seed = apply_reset_seed(seed, reset_seed);
                }
                vdf_sha(
                    &mut hasher,
                    &mut salt,
                    &mut seed,
                    config.num_checkpoints_in_vdf_step,
                    config.vdf_difficulty,
                    &mut checkpoints,
                );
                (
                    *checkpoints.last().unwrap(),
                    if i == steps.len() - 2 {
                        // If this is the last step, return the last checkpoint
                        Some(H256List(checkpoints))
                    } else {
                        // Otherwise, return just the seed for the next step
                        None
                    },
                )
            })
            .collect()
    });

    let last_step_checkpoints = test.last().unwrap().1.clone();
    let test: H256List = H256List(test.into_iter().map(|par| par.0).collect());

    let checkpoints_are_valid = test == vdf_info.checkpoints;

    if !checkpoints_are_valid {
        // Compare the original list with the calculated one
        warn_mismatches(&test, &vdf_info.checkpoints);
        return Err(eyre::eyre!("VDF checkpoints are invalid!"));
    }

    let last_step_checkpoints_are_valid = last_step_checkpoints
        .as_ref()
        .is_some_and(|cks| *cks == vdf_info.last_step_checkpoints);

    if !last_step_checkpoints_are_valid {
        // Compare the original list with the calculated one
        if let Some(cks) = last_step_checkpoints { warn_mismatches(&cks, &vdf_info.last_step_checkpoints) }
        return Err(eyre::eyre!("VDF last step checkpoints are invalid!"));
    }

    Ok(())
}

fn warn_mismatches(a: &H256List, b: &H256List) {
    let mismatches: Vec<(usize, (&H256, &H256))> =
        a.0.iter()
            .zip(&(b.0))
            .enumerate()
            .filter(|(_i, (a, b))| a != b)
            .collect();

    for (index, (a, b)) in mismatches {
        error!(
            "Mismatched hashes at index {}: expected {:?} got {:?}",
            index, a, b
        );
    }
}
