//! This crate provides functions and utilities for VDF (Verifiable Delay Function) operations,
//! including checkpoint validation and seed application.

use eyre::Context;
use irys_types::block_production::Seed;
use irys_types::{H256List, VDFLimiterInfo, VdfConfig, H256, U256};
use openssl::sha;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

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
) -> eyre::Result<()> {
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
        let reset_seed = vdf_info.seed;
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

#[cfg(test)]
mod tests {
    use base58::{FromBase58, ToBase58};
    use irys_types::ConsensusConfig;
    use tracing::debug;
    use tracing_subscriber::fmt::SubscriberBuilder;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    use super::*;

    #[tokio::test]
    async fn test_checkpoints_for_single_step_block() {
        // step: 44398 output: 0x893d
        let testnet_config = ConsensusConfig::testnet();
        let vdf_info = VDFLimiterInfo {
            output: to_hash("AEj76XfsPWoB2CjcDm3RXTwaM5AKs7SbWnkHR8umvgmW"),
            global_step_number: 44398,
            seed: to_hash("11111111111111111111111111111111"),
            next_seed: to_hash("11111111111111111111111111111111"),
            prev_output: to_hash("8oYs33LUVjtsB6rz6BRXBsVS48WbZJovgbyviKziV6ar"),
            // spellchecker:off
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
            // spellchecker:on
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };

        let mut config = testnet_config.vdf;
        config.sha_1s_difficulty = 100_000;

        let x = last_step_checkpoints_is_valid(&vdf_info, &config).await;
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

        let mut config = testnet_config.vdf;
        config.sha_1s_difficulty = 100_000;

        let x = last_step_checkpoints_is_valid(&vdf_info, &config).await;
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

        let x = last_step_checkpoints_is_valid(&vdf_info, &config).await;
        assert!(x.is_ok());
    }

    // one special case that do not apply reset seed
    #[tokio::test]
    async fn test_checkpoints_for_single_step_one() {
        let mut testnet_config = ConsensusConfig::testnet();
        testnet_config.vdf.sha_1s_difficulty = 100_000;

        let vdf_info = VDFLimiterInfo {
            output: H256(
                hex::decode("68230a9b96fbd924982a3d29485ad2c67285d76f2c8fc0a4770d50ed5fd41efd")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
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
                H256(
                    hex::decode("4db3d32b00a0905fed3829682bdd4ff03331056b3f6d5dc48ea5de80bec801ef")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("71ea0860b21198de9d2067ddb3171497ee06644c6c9ba6bcbe488bab276cfe54")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("b63abbb2ad58bcdbf8993d729eafc7cb98e1d9cd0bfe5fa58d1bcbf458960e3f")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("071d7e6137b497246f5bee5de2e117539039e9d1fc9e9a1b194e9a5511d580e6")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("bceeaf844df41a28ebfc7f9bf8dbac8954ace0901f66b6306755cd197e969162")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("b94c02ccaee286f01de98f22cdd754f6734d54f3b4869d86a140c0ba89ad5971")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("b6eac3c9641c2a742f348f7d0f2832be6dd1fd7dd35219ce3e26b1662ee71e10")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("51235b5c4ed65922cd05ad1b80829fe1841a8e2f67da2fb05573e662b9fe3963")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("90b3cf12c8d5a98d7a5032152452dde0e53978ef1aeabf9cf48410043d447470")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("53bc409264f5b4bc5d5ee7e513fa1f810b5097cea40799b23df387a329576ee2")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("f37b93f15643305b7c72d78be902f20236084989f7edce21bb39088656ed7127")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("13226e7fc7760c9f714907abda020f33cdb4b68e7514cacc607ece0854f6f9f2")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("a6093aacba1b19399152de271534c1d2f9e401d721baab93bc7b833ccaff74c5")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("aadc28a73c21da8f3a77e1eb390c57fbffed0979d27159bc18245ba2e57cec25")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("73fb959cc8fac89404721afc06e5971d23b4eca1f02d91d63e50a76539e08e0d")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("084c46940a567fc49ce9cdb0e3215220fb44f7053901a4db5f67ed8e21346a4d")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("3cbe5e09b6af0505b353b822a3e16ff7eb4fcd4abcf14c5f8524be4e71850d64")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("5b732beb962759353275535fc210ae2d81375a244f163c8e3b7b15de04b9664d")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("e7014083deac31f9f76d349cab861218d97dc3fa42d8c7ea95ec684cbc6c8227")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("69b75327dfd640f1d1ea7f27ad625a892a16ae2e6bbcf697e60d20e299fbf606")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("5d318b2fa14e19f78942100f15dddb92c93012302119bedcd8008ddec7faedbb")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("8cd38005367b8d9a96ccd08e37ea71b49adbfafd8b4fbc17826e680030ff36d8")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("fafad43cd0df6b0613c9ef89990ee3945bd69d73bfff7bf871863b8a38cc042c")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("41e7516f83863a1d82e73bcf2a60ffca0bc83371a8ce38960c6f337aadf861bb")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("68230a9b96fbd924982a3d29485ad2c67285d76f2c8fc0a4770d50ed5fd41efd")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            ]),
            steps: H256List(vec![H256(
                hex::decode("68230a9b96fbd924982a3d29485ad2c67285d76f2c8fc0a4770d50ed5fd41efd")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            )]),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        };

        let config = testnet_config.vdf;
        let x = last_step_checkpoints_is_valid(&vdf_info, &config).await;
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
