use crate::{Config, ANNUALIZED_COST_OF_OPERATING_16TB, GIGABYTE, MINER_PERCENTAGE_FEE, TERABYTE};
use eyre::ensure;
use rust_decimal::Decimal;

pub struct PriceCalc;

impl PriceCalc {
    const TB_REDUCE: u64 = 16; // used to scale from 16TB to 1TB
    const ANNUALIZED_COST_OF_STORING_1GB: f64 =
        ANNUALIZED_COST_OF_OPERATING_16TB / (Self::TB_REDUCE as f64 * (TERABYTE / GIGABYTE) as f64);

    // Get the current USD/IRYS exchange rate
    fn get_usd_to_irys_conversion_rate() -> f64 {
        // 1 USD = how many $IRYS. end result is in $IRYS
        1.0
    }

    /// Quote an IRYS price to store a number of bytes in perm storage
    pub fn calc_perm_storage_price(
        number_of_bytes_to_store: u64,
        config: &Config,
    ) -> eyre::Result<f64> {
        ensure!(config.chunk_size != 0, "Chunk size should not be 0");
        // $USD/GB
        let perm_cost = Self::calc_perm_cost_per_gb(
            config.decay_params.safe_minimum_number_of_years,
            config.decay_params.annualized_decay_rate.try_into()?,
            config.num_partitions_per_slot,
        )?;
        // $USD/GB
        let ingress_perm_fee = Self::calc_perm_fee_per_ingress_gb(
            config.storage_fees.number_of_ingress_proofs,
            config.storage_fees.ingress_fee,
        )?;
        // $IRYS/$USD
        let approximate_usd_irys_price = Self::get_usd_to_irys_conversion_rate();
        // Chunk
        let chunks = Self::get_chunks_from_bytes(number_of_bytes_to_store, config.chunk_size);
        // Chunk/GB
        let chunks_per_gb = f64::trunc(GIGABYTE as f64 / config.chunk_size as f64);
        // $USD/GB
        let immediate_miner_reward = perm_cost * MINER_PERCENTAGE_FEE;
        // Chunk/Chunk/GB * ($USD/GB + $USD/GB + $USD/GB) * $IRYS/$USD => GB * $USD/GB * $IRYS/$USD = $IRYS
        Ok((chunks as f64 / chunks_per_gb)
            * (ingress_perm_fee + perm_cost + immediate_miner_reward)
            * approximate_usd_irys_price)
    }

    // Data size to chunk count
    fn get_chunks_from_bytes(number_of_bytes_to_store: u64, chunk_size: u64) -> u64 {
        if number_of_bytes_to_store == 0 {
            return 0;
        }

        if number_of_bytes_to_store % chunk_size != 0 {
            number_of_bytes_to_store
                .checked_div(chunk_size)
                .expect("RHS is const")
                + 1
        } else {
            number_of_bytes_to_store
                .checked_div(chunk_size)
                .expect("RHS is const")
        }
    }

    // Calculate the decay rate for one GB of perm storage
    fn calc_perm_cost_per_gb(
        safe_minimum_number_of_years: u32,
        annualized_decay_rate: f64,
        partitions: u64,
    ) -> eyre::Result<f64> {
        ensure!(
            safe_minimum_number_of_years != 0,
            "Minimum number of years must be at least one"
        );
        ensure!(
            annualized_decay_rate > 0.0,
            "Decay rate must be non-zero and positive"
        );

        let total_cost = Self::ANNUALIZED_COST_OF_STORING_1GB
            * (1.
                - f64::powi(
                    1. - annualized_decay_rate,
                    safe_minimum_number_of_years as i32,
                ))
            / annualized_decay_rate;
        Ok(total_cost * partitions as f64)
    }

    // Calculate the perm fee for a number of ingress proofs
    fn calc_perm_fee_per_ingress_gb(
        ingress_proofs: u32,
        ingress_fee: Decimal,
    ) -> eyre::Result<f64> {
        ensure!(ingress_proofs != 0, "Ingress proofs must be > 0");
        let ingress_fee = f64::try_from(ingress_fee)?;
        Ok(Self::ANNUALIZED_COST_OF_STORING_1GB + ingress_fee * ingress_proofs as f64)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{storage_pricing::Amount, DecayParams, StorageFees};
    use approx::assert_abs_diff_eq;
    use k256::ecdsa::SigningKey;

    const EPSILON: f64 = 1e-9;

    fn get_expected_chunk_price() -> Option<f64> {
        // These values come from 200 years, 1% decay rate, n partitions, 5% miner fee
        const PRICE_FOR_1_PARTITION: f64 = 9.250481617324689e-5;
        const PRICE_FOR_10_PARTITIONS: f64 = 0.0006825861795615196;

        match get_config().num_partitions_per_slot {
            1 => Some(PRICE_FOR_1_PARTITION),
            10 => Some(PRICE_FOR_10_PARTITIONS),
            _ => None, // todo: if number of replicas, or partitions_per_slot are added, append them here
        }
    }

    fn get_config() -> Config {
        Config {
            // These are the params being tested.
            chunk_size: 256 * 1024,
            num_chunks_in_partition: 10,
            num_partitions_per_slot: 1,
            decay_params: DecayParams {
                safe_minimum_number_of_years: 200,
                annualized_decay_rate: rust_decimal_macros::dec!(0.01),
            },
            storage_fees: StorageFees {
                number_of_ingress_proofs: 10,
                ingress_fee: rust_decimal_macros::dec!(0.01),
            },

            // These params do not affect the tests.
            block_time: u64::default(),
            max_data_txs_per_block: u64::default(),
            difficulty_adjustment_interval: u64::default(),
            max_difficulty_adjustment_factor: rust_decimal::Decimal::default(),
            min_difficulty_adjustment_factor: rust_decimal::Decimal::default(),
            num_chunks_in_recall_range: u64::default(),
            vdf_reset_frequency: usize::default(),
            vdf_parallel_verification_thread_limit: usize::default(),
            num_checkpoints_in_vdf_step: usize::default(),
            vdf_sha_1s: u64::default(),
            entropy_packing_iterations: u32::default(),
            chain_id: u64::default(),
            capacity_scalar: u64::default(),
            num_blocks_in_epoch: u64::default(),
            submit_ledger_epoch_length: u64::default(),
            num_writes_before_sync: u64::default(),
            reset_state_on_restart: bool::default(),
            chunk_migration_depth: u32::default(),
            mining_key: SigningKey::from_slice(
                &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
                    .expect("valid hex"),
            )
            .expect("valid key"),
            num_capacity_partitions: None,
            port: u16::default(),
            anchor_expiry_depth: u8::default(),
            genesis_price_valid_for_n_epochs: u8::default(),
            genesis_token_price: Amount::token(rust_decimal_macros::dec!(1))
                .expect("valid token amount"),
            token_price_safe_range: Amount::percentage(rust_decimal_macros::dec!(1))
                .expect("valid percentage"),
            cpu_packing_concurrency: u16::default(),
            gpu_packing_batch_size: u32::default(),
        }
    }

    #[test]
    fn test_calc_perm_storage_price_0_bytes() {
        let bytes_to_store = 0;
        let config = get_config();
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = 0.0;
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_256_bytes() {
        let bytes_to_store = 256;
        let config = get_config();
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_chunk() {
        let config = get_config();
        let bytes_to_store = config.chunk_size;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_2_chunks() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 2;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = 2.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_mb() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 4;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = 4.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_gb() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 4 * 1024;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let expected = 4.0 * 1024.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_cost_per_gb_10_years() {
        let safe_minimum_number_of_years = 10;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gb(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        )
        .unwrap();
        assert_abs_diff_eq!(0.026294929372578817, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_200_years() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gb(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        )
        .unwrap();
        assert_abs_diff_eq!(0.23815558941406056, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_0_years() {
        let safe_minimum_number_of_years = 0;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gb(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        );
        assert_eq!(true, res.is_err())
    }

    #[test]
    fn test_calc_perm_cost_per_gb_0_decay_rate() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.0;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gb(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        );
        assert_eq!(true, res.is_err())
    }

    #[test]
    fn test_get_perm_fee() {
        let ingress_proofs = 10;
        let ingress_fee = rust_decimal_macros::dec!(0.01);
        let res = PriceCalc::calc_perm_fee_per_ingress_gb(ingress_proofs, ingress_fee).unwrap();
        assert_abs_diff_eq!(0.10275000000000001, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_chunks() {
        const ONE_KB: u64 = 1024;
        let chunk_size = get_config().chunk_size;

        let number_of_bytes_to_store = 0; // 0 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(0, res);
        let number_of_bytes_to_store = 1; // 1 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(1, res);
        let number_of_bytes_to_store = 256 * ONE_KB; // 256 KiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(1, res);
        let number_of_bytes_to_store = 257 * ONE_KB;
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(2, res);
        let number_of_bytes_to_store = 1_024 * ONE_KB; // 1 MiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(4, res);
        let number_of_bytes_to_store = 1_025 * ONE_KB;
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(5, res);
        let number_of_bytes_to_store = 1_073_741_824; // 1 GiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(4096, res);
        let number_of_bytes_to_store = 1_073_741_825; // 1 GiB + 1 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store, chunk_size);
        assert_eq!(4097, res);
    }
}
