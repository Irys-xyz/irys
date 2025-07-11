//! This crate provides functions and utilities for VDF (Verifiable Delay Function) operations,
//! including checkpoint validation and seed application.

use eyre::Context as _;
use irys_types::block_production::Seed;
use irys_types::{H256List, VDFLimiterInfo, VdfConfig, H256, U256};
use openssl::sha;
pub use rayon;
use rayon::prelude::*;
use sha2::{Digest as _, Sha256};
use std::time::Duration;

pub mod state;
pub mod vdf;
pub mod vdf_utils;

#[inline]
pub fn vdf_sha(
    hasher: &mut Sha256,
    salt: &mut U256,
    seed: &mut H256,
    num_checkpoints: usize,
    num_iterations_per_checkpoint: u64,
    checkpoints: &mut [H256],
) {
    let mut local_salt: [u8; 32] = [0; 32];

    for checkpoint_idx in 0..num_checkpoints {
        salt.to_little_endian(&mut local_salt);

        for _ in 0..num_iterations_per_checkpoint {
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
    num_iterations_per_checkpoint: usize,
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
        for _ in 1..num_iterations_per_checkpoint {
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

pub fn warn_mismatches(a: &H256List, b: &H256List) {
    let mismatches: Vec<(usize, (&H256, &H256))> =
        a.0.iter()
            .zip(&(b.0))
            .enumerate()
            .filter(|(_i, (a, b))| a != b)
            .collect();

    for (index, (a, b)) in mismatches {
        tracing::error!(
            "Mismatched hashes at index {}: expected {:?} got {:?}",
            index,
            a,
            b
        );
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
    config: &VdfConfig,
    reset_seed: H256,
) -> eyre::Result<()> {
    let last_step = vdf_info
        .steps
        .0
        .last()
        .ok_or(eyre::eyre!("No steps are found in the block"))?;
    let last_step_last_checkpoint = vdf_info
        .last_step_checkpoints
        .0
        .last()
        .ok_or(eyre::eyre!("No last checkpoints are found in the block"))?;
    if *last_step != vdf_info.output {
        return Err(eyre::eyre!("The last step does not match the output"));
    }
    if last_step != last_step_last_checkpoint {
        return Err(eyre::eyre!(
            "The last step does not match the last checkpoint"
        ));
    }

    let mut seed = if vdf_info.steps.len() >= 2 {
        vdf_info.steps[vdf_info.steps.len() - 2]
    } else {
        vdf_info.prev_output
    };
    let mut checkpoint_hashes = vdf_info.last_step_checkpoints.clone();

    let global_step_number: usize = vdf_info
        .global_step_number
        .try_into()
        .wrap_err("Should run in a 64 bits architecture!")?;

    // If the vdf reset happened on this step, apply the entropy to the seed (special case is step 0 that no reset is applied, then the > 1)
    if (global_step_number > 1) && ((global_step_number - 1) % config.reset_frequency == 0) {
        tracing::info!(
            "Applying reset step: {} seed {:?}",
            global_step_number,
            seed
        );
        seed = apply_reset_seed(seed, reset_seed);
    } else {
        tracing::info!(
            "Not applying reset step: {} seed {:?}",
            global_step_number,
            seed
        );
    };

    // Insert the seed at the head of the checkpoint list
    checkpoint_hashes.0.insert(0, seed);
    let cp = checkpoint_hashes.clone();

    // Calculate the starting salt value for checkpoint validation
    let start_salt = U256::from(step_number_to_salt_number(
        config,
        (global_step_number - 1) as u64,
    ));
    let config = config.clone();

    let test = tokio::task::spawn_blocking(move || {
        // Limit threads number to avoid overloading the system using configuration limit
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.parallel_verification_thread_limit)
            .build()
            .unwrap();

        let num_iterations = config.num_iterations_per_checkpoint();
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

    let is_valid = test == vdf_info.last_step_checkpoints;

    if !is_valid {
        // Compare the blocks list with the calculated one, looking for mismatches
        warn_mismatches(&vdf_info.last_step_checkpoints, &H256List(test));
        Err(eyre::eyre!("Checkpoints are invalid"))
    } else {
        Ok(())
    }
}

/// Derives a salt value from the `step_number` for checkpoint hashing
///
/// # Arguments
///
/// * `step_number` - The step the checkpoint belongs to, add 1 to the salt for
/// each subsequent checkpoint calculation.
pub const fn step_number_to_salt_number(config: &VdfConfig, step_number: u64) -> u64 {
    match step_number {
        0 => 0,
        _ => (step_number - 1) * config.num_checkpoints_in_vdf_step as u64 + 1,
    }
}

#[derive(Debug, Clone)]
pub struct VdfStep {
    pub step: H256,
    pub global_step_number: u64,
}

pub trait MiningBroadcaster {
    fn broadcast(&self, seed: Seed, checkpoints: H256List, global_step: u64);
}

/// Runs the VDF, starting with the testnet config's step count, calibrating the iterations between runs to get to ~1s/step - returning the value once `runs` runs are complete
/// Note: please set `runs` to a decently high value, so calibration can occur on the maximum sustained performance, instead of the short lived peak performance
/// This function may not accurately provide an iteration count for tests, or when many VDF threads are run at the same time
/// to calibrate that, run multiple instances of this function at the same time and take the lowest value
/// (this is particularly notable on Heterogenous CPUs, with some higher and some lower performance cores)
pub fn calibrate_vdf(runs: u64) -> u64 {
    // let precision_perc = 0.05;
    let mut config = irys_types::ConsensusConfig::testnet().vdf;
    let target_secs = Duration::from_secs(1).as_secs_f64();

    let mut seed: H256 = H256::zero();
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];

    // we don't early return once we have a precise value - this is to allow the calibration to accurately reflect the sustained performance
    for attempt in 0..runs {
        let mut hasher = Sha256::new();
        let mut salt = U256::from(0);

        let start = std::time::Instant::now();
        vdf_sha(
            &mut hasher,
            &mut salt,
            &mut seed,
            config.num_checkpoints_in_vdf_step,
            config.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let elapsed = start.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        let ratio = target_secs / elapsed_secs;

        println!(
            "attempt {}: {} iterations took {:.6}s (target: {:.6}s, ratio: {:.4})",
            attempt + 1,
            config.sha_1s_difficulty,
            elapsed_secs,
            target_secs,
            ratio
        );

        // // early return if our ratio is <= precision
        // if (ratio - 1.0).abs() <= precision_perc {
        //     println!(
        //         "calibration complete: {} iterations in {} attempts",
        //         attempt + 1,
        //         config.sha_1s_difficulty
        //     );
        //     return config.sha_1s_difficulty;
        // }

        // adjust iterations based on the ratio of target time to actual time
        config.sha_1s_difficulty = (config.sha_1s_difficulty as f64 * ratio).round() as u64;

        if config.sha_1s_difficulty == 0 {
            config.sha_1s_difficulty = 1;
        }
    }

    println!(
        "maximum attempts reached - best approximation: {}",
        config.sha_1s_difficulty
    );

    config.sha_1s_difficulty
}

#[cfg(test)]
mod tests {
    use super::*;
    use base58::{FromBase58 as _, ToBase58 as _};
    use irys_types::{BlockHash, ConsensusConfig};
    use tracing::debug;

    #[tokio::test]
    async fn test_checkpoints_for_single_step_block() {
        // step: 44398 output: 0x893d
        let testnet_config = ConsensusConfig::testnet();
        let vdf_info = VDFLimiterInfo {
            output: to_hash("Vk8iBZReeugJi3iJNNKX6ty9soW1t3N9dxoFPMTDdye"),
            global_step_number: 44398,
            seed: to_hash("11111111111111111111111111111111"),
            next_seed: to_hash("11111111111111111111111111111111"),
            prev_output: to_hash("8oYs33LUVjtsB6rz6BRXBsVS48WbZJovgbyviKziV6ar"),
            // spellchecker:off
            last_step_checkpoints: H256List(vec![
                to_hash("FX3wyTVoueFp5vMWtfmTMdoa2KHAmdTizSXvGqPJziV2"),
                to_hash("58KM3ADijBZhVsw5362XxQxUXT7TWEM97Gbkkrgzubs8"),
                to_hash("FYrSjPuALaT2YjpNpBpK1nBXAeeoM3Wfr2m8boPM7Vq9"),
                to_hash("2ZF35vRmwGtYNW8XLZXAhvASU9mEsN1BH6QMsr9aByKC"),
                to_hash("7GY4RcZseBadHvdpvjz2FpKA2LqYUSYtqhVFwiMzZ6no"),
                to_hash("8NsG4i7HK5PM3yGrpHJgEUMFBW18C6iV325payMGvbDm"),
                to_hash("8CFw78gk22URpSEvvi2RTYnRffwJpCAcSk7Cy7VcjxXr"),
                to_hash("Bf16WhYkF4F6HFcm4xsFS6UJYNdhKxVdF3iRAwArcrov"),
                to_hash("3jmN2bcVHLt2ah6UTFLsFQZfZRUqeuYn25w29GUnAUg4"),
                to_hash("Hv4rDECtEn9yCpyLAaPSUmw9pNd1gYePXWhYPR5y1Siz"),
                to_hash("2KpsVM1h3sUnuPBbWXXJk4iqnZChd9N1xPZQvMvFGb7W"),
                to_hash("a125oyEn49QFJZCCDyrc8ZMBGV8Rho5ydBNMJ6egSpw"),
                to_hash("7yqU8MeRwzdW7bAeXk4Ji2iUJEeu4bXoiLZi9tnRUpfB"),
                to_hash("AP8TVNarCHBix62jD7ubKC3zsiZa258Lp6bsEp7c8zzh"),
                to_hash("9rZGu4UwpGxo4i5E3TpHheaQBR9acURsJpNp2bjzTBX7"),
                to_hash("27rDPGMCHCWTu9RvmWSnB9bRQ2k9H8LSixXyiWQD6oh2"),
                to_hash("FyFdr6oF5LVu8o7QxnTK2xFZMagJKKUGEaFbBXnhn6Q1"),
                to_hash("51GyYjqHZ6Kz7NNSwEByJo4Fs3HbLt1x8MLS7n3TJ52P"),
                to_hash("H54wCCVdX34q6SHyfCQWerD77Zy1QhwgWHMUtMDEDcJx"),
                to_hash("DKab4Hzn3QAbb1UoJydgendjHXBYQyL8kMpdmfZmPutS"),
                to_hash("89S4ktrFRea7r7A1bjUCtta55eFdhtVLoKvvNh7tjZK4"),
                to_hash("3x2pKSDPnp9AyyKtZYfBcbnS2CA3xJBg5NMtM3EgEzRj"),
                to_hash("7ty5bgPV5bFFBvKsAR9gxivJdg2Wu2HRZUGtq77qrSuo"),
                to_hash("2pEWae5bZ6P7nMg3HCrUYUAKWhfRXGFwnpYCEFTUyen2"),
                to_hash("Vk8iBZReeugJi3iJNNKX6ty9soW1t3N9dxoFPMTDdye"),
            ]),
            steps: H256List(vec![to_hash("Vk8iBZReeugJi3iJNNKX6ty9soW1t3N9dxoFPMTDdye")]),
            // spellchecker:on
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };

        let mut config = testnet_config.vdf;
        config.sha_1s_difficulty = 100_000;

        let x = last_step_checkpoints_is_valid(&vdf_info, &config, BlockHash::zero()).await;
        if x.is_err() {
            debug!("{:?}", x);
        }
        assert!(x.is_ok());

        if x.is_ok() {
            println!("Checkpoints are valid!");
        } else {
            println!("Checkpoints are invalid!");
        }

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

    #[tokio::test]
    async fn test_checkpoints_for_single_step_block_before_reset() {
        let testnet_config = ConsensusConfig::testnet();
        // step: 44398 output: 0x893d
        // spellchecker:off
        let vdf_info = VDFLimiterInfo {
            output: to_hash("E5acT3ETbH5C2KHrqPogt6JcVbjjYQbjQFShiHqGGgug"),
            global_step_number: 44400,
            seed: to_hash("11111111111111111111111111111111"),
            next_seed: to_hash("11111111111111111111111111111111"),
            prev_output: to_hash("EchaEZLcR6Vw2rHz2CafaHxe75tj31A2WHmTVsTTS9Sp"),
            last_step_checkpoints: H256List(vec![
                to_hash("AbGjYT2pTQLmjNp9UnBvGidgczr53Pfv7AvNxW1iCFJi"),
                to_hash("2BAnqK4FN57xTMGDnt2PVNFrN59WP6HZvP5F9rq9VBiZ"),
                to_hash("H3sa7VKLsV7zFk6i2WYgf2zyAFDWGFgqD8FvesL3TE8D"),
                to_hash("BETn26rhVxX29vW1w773SctQnzWEGUHfth6QyfxB8Een"),
                to_hash("DeeYLjVPnScS5vVJUTrjGJbmm6CRp4z4ZckEy5t4RHhS"),
                to_hash("7b2DcEHX9sX2iQPPoJH3KAgYrceTuHoe4u7BivrA5YX2"),
                to_hash("6eLA4BBEGfS4F3kpmWkmj2iXj7WBoetByyHbNMeu1ihM"),
                to_hash("DzPbaQ5rQfgsT4HKPAvK87T7JqDni1KDXZfrHbNpf2QD"),
                to_hash("9VM4txeWEtEeD5Y3cVagVySSRHVyJ8RSVv68ojgpPNge"),
                to_hash("4U6w56JrakUV3gSdY5FHE33WpxAtGCP4m6utGYeKSpTv"),
                to_hash("EmxkxKjhv7eHRofc5vcZ6UouUDQPmrBYb7bt7G4n8d7T"),
                to_hash("FnHxPS8cun2fY2WE13txBJDhvggdLGPRtdmUCpPELA1G"),
                to_hash("97BHjSMf69vX5NadDAxLoR7smaDpNFd9QzAwaizFw1Qp"),
                to_hash("54HSZkJuG1QEPXP4xqKgYSDYntocpAcHTQM8kCiQT4t6"),
                to_hash("HRtTJScmYdKRTWcyMQnmDQ3RFcJMHo4FndVGmZokb8wB"),
                to_hash("AiX8pZErGzdZPCQNWqCkucuPiZy3Y6jfJd9hZJZNadVn"),
                to_hash("CwT6kNSBTcZivunDJyfPKXxYhonV6LGgAboyx92jA1an"),
                to_hash("EsSXCJ13vJQXaNRzXUMWRu5HtFv9aZ4YWKmCGqqTmkT7"),
                to_hash("5Epgv3cn2yprjQHyYdBzRiadJuYc5nRYqafowCB43gBj"),
                to_hash("BCo54qTaBtZYNd8YJgCuFsv1rPPHtKmFEYBnsXJNBQbR"),
                to_hash("Afociuwn5ojvimD5KPJLVCPvxF3hJnVwZohYgXrUDZBf"),
                to_hash("5Z26M1BAWjVP2MVjHSuoXydXcPmiKrmbFUiBsqJaPHCg"),
                to_hash("XkuRhYUwaoramqEhpiDqYkhQNPQFQjZk9yQfmfeQoJ9"),
                to_hash("5rgebnYJLTTyVt9Bm1a7F7KkATwgYEZHCTw2cQ9xb7ow"),
                to_hash("E5acT3ETbH5C2KHrqPogt6JcVbjjYQbjQFShiHqGGgug"),
            ]),
            steps: H256List(vec![to_hash(
                "E5acT3ETbH5C2KHrqPogt6JcVbjjYQbjQFShiHqGGgug",
            )]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };
        // spellchecker:on

        let mut config = testnet_config.vdf;
        config.sha_1s_difficulty = 100_000;

        let x = last_step_checkpoints_is_valid(&vdf_info, &config, BlockHash::zero()).await;
        assert!(x.is_ok());

        if x.is_ok() {
            println!("Checkpoints are valid!");
        } else {
            println!("Checkpoints are invalid!");
        }

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

    #[tokio::test]
    async fn test_checkpoints_for_single_step_block_after_reset() {
        let mut testnet_config = ConsensusConfig::testnet();
        testnet_config.vdf.sha_1s_difficulty = 100_000;

        // step: 44398 output: 0x893d
        let vdf_info = VDFLimiterInfo {
            output: to_hash("3Qsb2Usx5679XnfZBr6VQNcMnJG1yMtvhHNYsKgefEL1"),
            global_step_number: 44401,
            seed: to_hash("11111111111111111111111111111111"),
            next_seed: to_hash("11111111111111111111111111111111"),
            prev_output: to_hash("E5acT3ETbH5C2KHrqPogt6JcVbjjYQbjQFShiHqGGgug"),
            last_step_checkpoints: H256List(vec![
                to_hash("HKXZ8gTfzbjKX1Yj4BiPYr2szt4tXiJ8XYMyLKMkzgYK"),
                to_hash("Hqx8kaHFmN8to736An11HrX612zwVFgBatCr5jym6URT"),
                to_hash("HFa2nA8igin9FNbYo2Y9zvjF7VEj7tsGcPcaK9zykfk3"),
                to_hash("2vxhZHfSytevAjFjPDsPc4KFBhxRpBxcPZB2RC8RYErz"),
                to_hash("hoySANowfuK5MBZA9uVBkp53tVWcSGnguvrTqsUevmo"),
                to_hash("4uHW4CMNvVfMbYPu4fmzUmBmJcjUrkYv88AUhzgY6Lrr"),
                to_hash("UbCkSxgPhkTzuR7JpuPv5iVT4S8to76WQU99ncKzSvS"),
                to_hash("9u5q95KhD8t3PULQmGox87idU3skP1VwjvL8MM6D2efQ"),
                to_hash("C2PqBj1jSktvCVk4HPj3C1axY1zgXzpbEbY1Ed1QRsuq"),
                to_hash("2SJJA3GhVmtyo2LJei1QHLiDrDPFdYVEz1FEcBHqhGKy"),
                to_hash("EgvP8KFPEu7oxqSF13xnhiAqQHZKqeVCTgCAMKLphh3A"),
                to_hash("4VVjwiNebvpKbXjfdSVFWSAf5T6SJbXBc7VhNvBQ8QcB"),
                to_hash("5m5DQ8eZ92C765aY6HWz1s8ijgGiBKNZwNGrfzRNf1XT"),
                to_hash("6U2Z7WxEbTsqyhrsKNcb2n1airWc4HCQxyKJV5oDK1xj"),
                to_hash("CA2xn9QAWPgLwkCFBPUkhM67GMZTcTy8DVvZuhfYrXVv"),
                to_hash("7b5U3EMh6ju6MT2DhWjxLfyJZopyYQceB9hzPsxH3cft"),
                to_hash("Ai21SQ9W4QwmKU4zamogrnsX3nUUQjTrMGnH5G22T5Qg"),
                to_hash("3QvcxYH1BHfxLiPs8NrNmj99jgZcufdVKRkn2BQNzT3F"),
                to_hash("F3Usefji2NMkauufHgL67pqdcEmAjVbtNwHRz4rJD9Mf"),
                to_hash("GgCyhokL4jnmr4XUJadNmvzkiATkYkBnDvqGbJvMNX45"),
                to_hash("7hWVtio9um364gPoHZZqVNqdRM7kivryCygUPbwSwS2N"),
                to_hash("F3fhBBUJPWqrmybCuzCgYiW5BZd7Xy22NxcCmb4pTsPc"),
                to_hash("D7xxh4WbC6PGzZEtc7LA4amcEDwsX4kbrYGb43vkQsqP"),
                to_hash("4rxCqPkfJhqjhNK6jCU2MtrauPACJ4yP7G8GEfXsuJCz"),
                to_hash("3Qsb2Usx5679XnfZBr6VQNcMnJG1yMtvhHNYsKgefEL1"),
            ]),
            steps: H256List(vec![to_hash(
                "3Qsb2Usx5679XnfZBr6VQNcMnJG1yMtvhHNYsKgefEL1",
            )]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };

        debug!("{:?}", vdf_info);

        let config = testnet_config.vdf;

        let x = last_step_checkpoints_is_valid(&vdf_info, &config, BlockHash::zero()).await;
        assert!(x.is_ok());
    }

    // one special case that do not apply reset seed
    #[tokio::test]
    async fn test_checkpoints_for_single_step_one() {
        let mut testnet_config = ConsensusConfig::testnet();
        testnet_config.vdf.sha_1s_difficulty = 100_000;

        // spellchecker:off
        let vdf_info = VDFLimiterInfo {
            output: to_hash("9RhHQbUYdcui4CpE2HGXQAvnT3ynP14RLvF6cYCLkXuG"),
            global_step_number: 1,
            seed: H256(
                hex::decode("0000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            next_seed: H256(
                hex::decode("0000000000000000000000000000000000000000000000000000000000000000")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            prev_output: H256(
                hex::decode("ca4d22678f78b87ee7f1c80229133ecbf57c99533d9a708e6d86d2f51ccfcb41")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            last_step_checkpoints: H256List(vec![
                to_hash("E6hG1Twshtuw3vrWybNyNGd1RDcGuDayF8j6SFNoEPek"),
                to_hash("BM3aS9t2wgRco1mt6uvzS5TWvAGUz5cUczp1rAUN2BK1"),
                to_hash("BUwMxGYGfYpEnW6QjXyVzeAr4MvsCMFhjsBFCpLhnh2L"),
                to_hash("GzjJMGybbGGewCwKzYYaZyMGXMA4pLgrssf1fLeT2RME"),
                to_hash("BokQ4zJbhtKbQmUtQqFNLGgDRZ8X2anf7FQvP5xr4wn9"),
                to_hash("HGABETU5MMqPGiyaDMtymSPNzXYuvuTMMGtxDYW7QqpG"),
                to_hash("BL3byeTWd9A8fTMVrHDTNMxbCfgiTDFfo3p3hKJ1PUcE"),
                to_hash("FBDWtGFJMHfed1ypZytZAaZ5duZ669PxE9K2aa2i2fvT"),
                to_hash("7zChTjDdr3RfViEKoSHy7bfBnzrU8bjmU1qxufMtKt9U"),
                to_hash("DCazGAztoKAdzsQodwQSK9SyeKFE35irPHcMhKfwtxbY"),
                to_hash("8ZVY1RPaFZm1EXZv9ft939B1969hmUj8My2ecNU4kpVj"),
                to_hash("J4aUqMRPrKPW3VsmQ3ZfFuUN8ekF8BxbPLdRagKS8pK1"),
                to_hash("CwdhRQ6U7yK7cjcnCSbpJ5aXf67Ei9rkckfnTigHDW4D"),
                to_hash("FbXnYikZXPS3V1g7Z4sv4GYPTUZ7PrdDNEXj54WqoTqs"),
                to_hash("5cUT6WESQtcrX36U3sAWMFejkk5UDX4AfEre4hpDYx6G"),
                to_hash("EA6zAro7R83xbV1fvjGFdsPepVjsBTcUY3ZTBYjXYsyw"),
                to_hash("BQWpwmsg718ZUTc5QZViUKDxsCMCCW2kzL4Yb6PCrFrS"),
                to_hash("8bD8azToVs69fNX1NHs1Zi4iTbfCnRmNLvQ2keg3fxPo"),
                to_hash("4r6WfULFkheaTKcyCtTrBALJ9XqoFCN8tYnmBiNb5oq8"),
                to_hash("41khgaNnxB9K71WS8b9RNendD3e3BrY9sXG56RAwNrbN"),
                to_hash("8EkaYS2FTQxrLQmkwXKApLqTJVkHGVNErSDNjy9cwmFQ"),
                to_hash("FtyNJMZJpAhwXbvTsPntMGCneG8TpgtiQvnhhTQqmEyy"),
                to_hash("D1boDbg56PZX2PyQiuRJPXwnLBqVmzafo823mUtYHAQY"),
                to_hash("62Pdx9FsHNFLafhqPiokhcdWZbXGb4T57ZswdHVmtL1z"),
                to_hash("9RhHQbUYdcui4CpE2HGXQAvnT3ynP14RLvF6cYCLkXuG"),
            ]),
            steps: H256List(vec![to_hash(
                "9RhHQbUYdcui4CpE2HGXQAvnT3ynP14RLvF6cYCLkXuG",
            )]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };
        // spellchecker:on

        let config = testnet_config.vdf;
        let x = last_step_checkpoints_is_valid(&vdf_info, &config, BlockHash::zero()).await;
        assert!(x.is_ok());

        if x.is_ok() {
            println!("Checkpoints are valid!");
        } else {
            println!("Checkpoints are invalid!");
        }

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
}
