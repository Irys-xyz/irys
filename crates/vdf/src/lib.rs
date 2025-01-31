//! This crate provides functions and utilities for VDF (Verifiable Delay Function) operations,
//! including checkpoint validation and seed application.

use base58::ToBase58;
use irys_types::{H256List, VDFLimiterInfo, VDFStepsConfig, H256, U256};

use nodit::interval::ii;
use openssl::sha;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tracing::{debug, error, info};
use vdf_state::VdfStepsReadGuard;

pub mod vdf_state;

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

/// Validates VDF `last_step_checkpoints` in parallel across available cores.
///
/// Takes a `VDFLimiterInfo` from a block header and verifies each checkpoint by:
/// 1. Getting initial seed from previous vdf step or `prev_output`
/// 2. Applying entropy if at a reset step
/// 3. Computing checkpoints in parallel using configured thread limit
/// 4. Comparing computed results against provided checkpoints
///
/// Returns Ok(()) if checkpoints are valid, Err otherwise with details of mismatches.
pub async fn last_step_checkpoints_is_valid(
    vdf_info: &VDFLimiterInfo,
    config: &VDFStepsConfig,
) -> eyre::Result<()> {
    let mut seed = if vdf_info.steps.len() >= 2 {
        vdf_info.steps[vdf_info.steps.len() - 2]
    } else {
        vdf_info.prev_output
    };
    let mut checkpoint_hashes = vdf_info.last_step_checkpoints.clone();

    // println!("---");
    // for (i, step) in vdf_info.steps.iter().enumerate() {
    //     println!("step{}: {}", i, Base64::from(step.to_vec()));
    // }
    // println!("seed: {}", Base64::from(seed.to_vec()));
    // println!(
    //     "prev_output: {}",
    //     Base64::from(vdf_info.prev_output.to_vec())
    // );

    // println!(
    //     "cp{}: {}",
    //     0,
    //     Base64::from(vdf_info.last_step_checkpoints[0].to_vec())
    // );
    // println!(
    //     "cp{}: {}",
    //     24,
    //     Base64::from(vdf_info.last_step_checkpoints[24].to_vec())
    // );

    let global_step_number: usize = vdf_info.global_step_number as usize;

    // If the vdf reset happened on this step, apply the entropy to the seed
    if global_step_number % config.vdf_reset_frequency == 0 {
        let reset_seed = vdf_info.seed;
        seed = apply_reset_seed(seed, reset_seed);
    }

    // Insert the seed at the head of the checkpoint list
    checkpoint_hashes.0.insert(0, seed);
    let cp = checkpoint_hashes.clone();

    // Calculate the starting salt value for checkpoint validation
    let start_salt = U256::from(step_number_to_salt_number(
        config,
        (global_step_number - 1) as u64,
    ));
    let config = config.clone();

    let test = actix_rt::task::spawn_blocking(move || {
        // Limit threads number to avoid overloading the system using configuration limit
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.vdf_parallel_verification_thread_limit)
            .build()
            .unwrap();

        let num_iterations = config.vdf_difficulty;
        let test: Vec<H256> = pool.install(|| {
            (0..config.num_checkpoints_in_vdf_step)
                .into_par_iter()
                .map(|i| {
                    let mut salt_buff: [u8; 32] = [0; 32];
                    (start_salt + i).to_little_endian(&mut salt_buff);
                    let mut seed = cp[i];
                    let mut hasher = Sha256::new();

                    for _ in 0..num_iterations {
                        hasher.update(salt_buff);
                        hasher.update(seed.as_bytes());
                        seed = H256(hasher.finalize_reset().into());
                    }
                    seed
                })
                .collect::<Vec<H256>>()
        });
        test
    })
    .await?;

    // TODO: Remove these only for debugging the test below
    for (i, checkpoint) in checkpoint_hashes.iter().enumerate() {
        println!("{}: {}", i, checkpoint.0.to_base58());
    }

    for (i, checkpoint) in test.iter().enumerate() {
        println!("{}: {}", i, checkpoint.0.to_base58());
    }

    let is_valid = test == vdf_info.last_step_checkpoints;

    if !is_valid {
        // Compare the blocks list with the calculated one, looking for mismatches
        warn_mismatches(&vdf_info.last_step_checkpoints, &H256List(test));
        Err(eyre::eyre!("Checkpoints are invalid"))
    } else {
        Ok(())
    }
}

/// Validate the steps from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
///
/// # Arguments
///
/// * `vdf_info` - The Vdf limiter info from the block header to validate.
///
/// # Returns
///
/// - `bool` - `true` if the steps are valid, false otherwise.
pub fn vdf_steps_are_valid(
    vdf_info: &VDFLimiterInfo,
    config: &VDFStepsConfig,
    vdf_steps_guard: VdfStepsReadGuard,
) -> eyre::Result<()> {
    info!(
        "Checking seed {:?} reset_seed {:?}",
        vdf_info.prev_output, vdf_info.seed
    );

    let start = vdf_info.global_step_number - vdf_info.steps.len() as u64 + 1 as u64;
    let end: u64 = vdf_info.global_step_number;

    match vdf_steps_guard.read().get_steps(ii(start, end)) {
        Ok(steps) => {
            debug!("Validating VDF steps from VdfStepsReadGuard!");
            if steps != vdf_info.steps {
                warn_mismatches(&steps, &vdf_info.steps);
                return Err(eyre::eyre!("VDF steps are invalid!"));
            } else {
                // Do not need to check last step checkpoints here, were checked in pre validation
                return Ok(())
            }
        },
        Err(err) =>
            debug!("Error getting steps from VdfStepsReadGuard: {:?} so calculating vdf steps for validation", err)
    };

    let reset_seed = vdf_info.seed;

    let mut step_hashes = vdf_info.steps.clone();

    // Add the seed from the previous nonce info to the steps
    let previous_seed = vdf_info.prev_output;
    step_hashes.0.insert(0, previous_seed);

    // Make a read only copy for parallel iterating
    let steps = step_hashes.clone();

    // Calculate the step number of the first step in the blocks sequence
    let start_step_number: u64 = vdf_info.global_step_number - vdf_info.steps.len() as u64;

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
                    && (start_step_number + i as u64) % config.vdf_reset_frequency as u64 == 0
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

    let steps_are_valid = test == vdf_info.steps;

    if !steps_are_valid {
        // Compare the original list with the calculated one
        warn_mismatches(&test, &vdf_info.steps);
        return Err(eyre::eyre!("VDF steps are invalid!"));
    }

    let last_step_checkpoints_are_valid = last_step_checkpoints
        .as_ref()
        .is_some_and(|cks| *cks == vdf_info.last_step_checkpoints);

    if !last_step_checkpoints_are_valid {
        // Compare the original list with the calculated one
        if let Some(cks) = last_step_checkpoints {
            warn_mismatches(&cks, &vdf_info.last_step_checkpoints)
        }
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

#[cfg(test)]
mod tests {
    use base58::FromBase58;

    use super::*;

    #[tokio::test]
    async fn test_checkpoints_for_single_step_block() {
        let vdf_info = VDFLimiterInfo {
            output: to_hash("AEj76XfsPWoB2CjcDm3RXTwaM5AKs7SbWnkHR8umvgmW"),
            global_step_number: 44398,
            seed: to_hash("11111111111111111111111111111111"),
            next_seed: to_hash("11111111111111111111111111111111"),
            prev_output: to_hash("8oYs33LUVjtsB6rz6BRXBsVS48WbZJovgbyviKziV6ar"),
            last_step_checkpoints: H256List(vec![
                to_hash("5YGk1yQMi5TwLf2iHAaLWnS8iDSdzQEhm3fambxy5Syy"),
                to_hash("FM8XvtafL5pEsMkwJZDEZpppQpbCWQHxSSijffSjHVaX"),
                to_hash("6YcQjupYZRGN5fDmngnXLmtWbZUDU2Ur6kgH1N8pW4hX"),
                to_hash("B85pKLNbsj2YysNk9gs3bHDJN6YuHWCAdjhV9x7WteJr"),
                to_hash("9xH77QvzHDvmptkefM39NSU7dVNHHjUMw4Wyz31GXSbY"),
                to_hash("mmkTrT6cDFsx8XzCCs8t7CHmAGbbepV2NA2fYH89ywp"),
                to_hash("Df4f7UDTykXDLbPYPxiWX9HBndHnUEFhVB9hBmBGt4wb"),
                to_hash("B7Rf1wEC8QLDfR3vD4fdVdvaHhbBz1Nd1r9KPpUbwrJp"),
                to_hash("4BxEQ8GUBEWn5NQfSXuWiPPyW6gensivj2JQognZ8tKw"),
                to_hash("G1N6N8nXLF4SCVkspJ5cbK7isxcHJqhoQMin99p7hvov"),
                to_hash("F1JPya8vsK3JeCJNDTZhESqhr6BjUSVzvdiMzmaDbjHE"),
                to_hash("5bJKjJMyNKBP42E8FuEYMPJeFXBFHvVN9d6nuTMNk1Gy"),
                to_hash("4iHkRQrhRabYZtRuJhZkMTY9QX2cpM2RDN5s5d15oXtR"),
                to_hash("3mmV4etPnrpCJZ1pXj2LbYaCX6L2ymbkZLnMMhQdAUUM"),
                to_hash("3aqYUYxzQr2bgPPk4s81AdGS6ekEsZNsK4yYwy2Sc86m"),
                to_hash("Fxz3fgD6e3VS2Ka5fWQ3rFqzNdSPxctX84MwrR8D9pw4"),
                to_hash("3VALw7Y6pxCbGTuCBWFonWKnBakSYb3vCoVKHZWGD9gM"),
                to_hash("8vE2CgMn4Est5rFjVTfBqe1fwUVZryKPAfxzx24iccxh"),
                to_hash("HGRDCe81gGqF1FidJf6Mwt6GiYFyDyUkeLQUQxy72GeF"),
                to_hash("3XYHLLZynkE4gL8cL1e4qmn6pg2vUCEbDL1ySVExZXnw"),
                to_hash("93HH3c29jVch3n3jxSTAgveq4MPNJAsmKdBGL7u75twh"),
                to_hash("3n4YuWzgTNpDy3PVQ2w8NwwhaY8ZQkx68UrSQeNG7Nad"),
                to_hash("DnALYWSXJpJkzg4ucVqC71o6dLMz48uaLrM5EnbVJN5U"),
                to_hash("B1fEtkY9wJ45SRAJhy7GfsML2Sbbjh5m56tzDWSw9EUA"),
                to_hash("AEj76XfsPWoB2CjcDm3RXTwaM5AKs7SbWnkHR8umvgmW"),
            ]),
            steps: H256List(vec![to_hash(
                "AEj76XfsPWoB2CjcDm3RXTwaM5AKs7SbWnkHR8umvgmW",
            )]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };
        let config = VDFStepsConfig::default();

        let x = last_step_checkpoints_is_valid(&vdf_info, &config).await;

        println!(
            "---\nsteps: {} last_checkpoint: {}\n seed: {}",
            vdf_info.steps[0].0.to_base58(),
            vdf_info
                .last_step_checkpoints
                .0
                .last()
                .unwrap()
                .0
                .to_base58(),
            vdf_info.prev_output.0.to_base58()
        );
        println!("x: {:?}", x);
    }

    fn to_hash(base_58_hash: &str) -> H256 {
        H256(base_58_hash.from_base58().unwrap().try_into().unwrap())
    }
}
