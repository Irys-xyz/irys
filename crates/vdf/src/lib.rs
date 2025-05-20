//! This crate provides functions and utilities for VDF (Verifiable Delay Function) operations,
//! including checkpoint validation and seed application.

use eyre::Context;
use irys_types::{H256List, VDFLimiterInfo, VdfConfig, H256, U256};
use openssl::sha;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

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

        let num_iterations = config.sha_1s_difficulty;
        let test: Vec<H256> = pool.install(|| {
            (0..config.num_checkpoints_in_vdf_step)
                .into_par_iter()
                .map(|i| {
                    let mut salt_buff: [u8; 32] = [0; 32];
                    (start_salt + i).to_little_endian(&mut salt_buff);
                    let mut seed = cp[i];
                    let mut hasher = Sha256::new();

                    for _ in 0..num_iterations {
                        hasher.update(&salt_buff);
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

    // println!("test{}: {}", 0, Base64::from(test[0].to_vec()));
    // println!("test{}: {}", 24, Base64::from(test[24].to_vec()));

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

#[cfg(test)]
mod tests {
    use base58::{FromBase58, ToBase58};
    use irys_types::ConsensusConfig;

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
            output: H256(
                hex::decode("d146d9bf9d6d1e5e1dde15b7fe11c72928d9809587758f1f9a343146dc0d60c4")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            global_step_number: 44400,
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
                    hex::decode("01cbc7f0c9a0a9bc02f8de61c806c227865977f1288085a5ec8ee63f117f1441")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("c9ae2ca12ebe463d076e58b36d79ed826e4cf1518bfa4497f9dd600efb149ef7")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("a667ba60fcc78ff881be5a4362ee8ed620d40cd8e1f243a9d3784d49440ad568")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("02bcafa19e4c87deafe4a774a627439199de1a6cdfcfe3fcb4f28c3904cab2db")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("6e69f859b49d3dd83c3d8bf63b1343b6705e21baff18ea903274e5cf86df0942")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("b9bb2804a759e32dc78d6e66bb9083524426bb21d401bba1d10be8720df1c2aa")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("24047f41de57234304e8f9c417e860fa52efc6902f576b9f3ee5aab0500076d3")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("14b032ad411fbb559be2601d6dbf58f0be199a8f1fbd727939691c6f628d66de")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("f4e34ef580c5e603bcb89edd010d78a562999f556d7c5c042c2900a6a7a00dc7")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("7a695bef7b4a042852515e4df71a9267131a3f68ee180ef003ba2a8769026d4e")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("1d6a6344c7ef1b8a2b1f05755d576b44142d7390d6b8a874f5d52a15988027d1")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("aa26de9d3a67fd78d38f98974dfaf7a095c8884363fe861750420e5bd564ceb0")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("ce785db61216b15f4f81b4f2d61c04c340e8576aecc7db85a0e1e0f3092bbf71")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("dd56b778c580cc446aae146209d48a76873ef55882b036e73f5ced4e0e630516")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("03013a92e391d608d468e15bbea0517b0d1619b00d76a41ad9c8bcc93e90964a")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("6e445f49a57953dfd586609dab877177cf02c21fbf93854dc69771a886e4a05b")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("5cb45592f6fd08fdd29ad661a3202c5c8b6bab2694bfbd610435ad48db811eba")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("96ecd10174dadd788f82850ae8b1d45bd17289133e2eb77e625eca1c8e5c0031")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("046a20ecbbd60459c80d6c69ae1649e9cffc6faa037cd6c4a904365142812aae")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("6f943113534cadafcc07cbf5d43703e88029f91af140c2cc595bcf17df9703fd")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("72984524bd2c3e57f2f2e4d35ccf7d03febb23be91ec53d634cc28469d784fec")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("32d91ef55df8b24dff40dd6131b7d962638f47d0ce37a73143734d5b39d71fd0")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("e8ead73c39047f7a4b37f005fd5a83ee89493c35a17bf3fbf2176df4904d8acc")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("88d48a1daf73a989503136fd9ed5633853e8554271ec69a94be52a45c8c13ad1")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("d146d9bf9d6d1e5e1dde15b7fe11c72928d9809587758f1f9a343146dc0d60c4")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            ]),
            steps: H256List(vec![H256(
                hex::decode("d146d9bf9d6d1e5e1dde15b7fe11c72928d9809587758f1f9a343146dc0d60c4")
                    .unwrap()
                    .try_into()
                    .unwrap(),
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
            output: H256(
                hex::decode("4a6976fc0b102ea04a6e2073c0db72ec763dd6cd53346b7b36592dc415cda46f")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            global_step_number: 44401,
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
                hex::decode("d146d9bf9d6d1e5e1dde15b7fe11c72928d9809587758f1f9a343146dc0d60c4")
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            last_step_checkpoints: H256List(vec![
                H256(
                    hex::decode("5dfcd481cc2fc3ed4198d2c0001ce9ef053495f5f8db2aaf2cbf591f507efe76")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("62cf2b21e9dacce5bcadbc4846466542e2aaf8a4f054634f2fbbe97d7559e377")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("cb8775eb12ac605e9b6141397f1f07681e5c8e29608d3aff13b9348c23803311")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("3eee39dbf370d5a7c3454350034ba9fd8d5dff4a6564634d728e69d56c1601b7")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("35f10861be8cdb13eb8a690e53a9885f6dc36ff5dd8c87fb5b87584962ff435f")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("a8e6cba62e1092006db8d68c7adb68a929d00e191f1924b17b9beb026cdba244")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("12636a5e00003b54b51cd5d44d44e4667a20cc2565da98c4f93e636a90da243c")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("22a778ddeaccac5d84789a3aa58ecbab1fbb2a572b755608ededb5085b990eea")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("518dbdfaaa5b1864ddd90ad3f0d4bc2a86e1d62e4b595f4d5a4b8f88d5e773a2")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("9195a55e29a902acce59308d6fdbb348a25fb6d8f69a4a2f4523aa08e211fb5f")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("c65d8943868696aa03be0b5eaa85e0b6626d0ffccda40aa141d07054bebd62e6")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("dbc9989d645291aa7d14bd3bb4725c4c3029a986dcdd9d97b8d63241cdc35626")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("e33edab2786b7675a6a2cbbd9fd5a5cd9b2fb7aafce7f0ac90e42be6ac5863e6")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("62f9dff22d67198950f457afda82347f00f23f8e6bdc62d2a1fd4cd0f2d23312")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("c2b95488c1c225bce89b1cc29dda710d64bc5f16c4f32cce01eb9a3e67139a06")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("f2158c0f31f7301e0c3f09e233a42ff50c33f1f136b503b13adcd914acca438c")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("fa4841e900ed9e778a86161914a412a39031565d647af1f97563d4f3c9c6fa62")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("3417b742c8558d78180bc51b7ddf138e9378ec8286cbd99029bb135c7fdd7304")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("cec3c7b680bfae2d5356b48288f55649ebae3e1bff242ccd774e55b998b1b670")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("07c95adccfc38082f79ee0b110c53f4a9a7c3048397844a8bc7bdb72167376c5")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("cf1a222eb815146a4d1377c7fecbbb52a0d9cd66eea60b00bef94cf28d3f77a3")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("bb85423727c38952d695fbe40687d327053582b9cc180c0108b08c40be5c16dc")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("742ad9949bb21534f01e96d80a6c1d495f82fec13cd6b7ff9924bf5da4439b68")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("8179cd7681b9ca617a3c65510b280c7f4c123f30bb0d6f5e9de9768cabcc42a2")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
                H256(
                    hex::decode("4a6976fc0b102ea04a6e2073c0db72ec763dd6cd53346b7b36592dc415cda46f")
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            ]),
            steps: H256List(vec![H256(
                hex::decode("4a6976fc0b102ea04a6e2073c0db72ec763dd6cd53346b7b36592dc415cda46f")
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
