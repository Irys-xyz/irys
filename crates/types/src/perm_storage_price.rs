use crate::{Config, ANNUALIZED_COST_OF_OPERATING_16TB, GIBIBYTE, MINER_PERCENTAGE_FEE, TEBIBYTE};
use eyre::{ensure, OptionExt};
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;

const SCALE_FACTOR: Decimal = dec!(1e18);

pub struct PriceCalc;

impl PriceCalc {
    const TB_REDUCE: Decimal = dec!(16); // used to scale from 16TB to 1TB

    // Get the current USD/IRYS exchange rate
    fn get_usd_to_irys_conversion_rate() -> Decimal {
        // 1 USD = how many $IRYS. end result is in $IRYS
        dec!(1)
    }

    /// Quote an IRYS price to store a number of bytes in perm storage
    pub fn calc_perm_storage_price(
        number_of_bytes_to_store: u64,
        config: &Config,
    ) -> eyre::Result<u64> {
        ensure!(config.chunk_size != 0, "Chunk size should not be 0");
        let annualized_cost_of_storing_1gib = ANNUALIZED_COST_OF_OPERATING_16TB
            .checked_div(
                (Self::TB_REDUCE.checked_mul(Decimal::try_from(TEBIBYTE / GIBIBYTE)?))
                    .ok_or_eyre("Decimal operation failed")?,
            )
            .ok_or_eyre("Decimal operation failed")?;

        // $USD/GiB
        let perm_cost = Self::calc_perm_cost_per_gib(
            Decimal::from(config.decay_params.safe_minimum_number_of_years),
            config.decay_params.annualized_decay_rate,
            Decimal::from(config.num_partitions_per_slot),
            annualized_cost_of_storing_1gib,
        )?;
        // $IRYS/$USD
        let approximate_usd_irys_price = Self::get_usd_to_irys_conversion_rate();
        let perm_cost_per_gib_irys = perm_cost
            .checked_mul(approximate_usd_irys_price)
            .ok_or_eyre("Decimal operation failed")?;
        let num_chunks = Self::get_chunks_from_bytes(number_of_bytes_to_store, config.chunk_size);
        let chunks_per_gib = f64::trunc(GIBIBYTE as f64 / config.chunk_size as f64);
        // $USD
        let base_fee_irys = Decimal::try_from(num_chunks as f64 / chunks_per_gib)?
            .checked_mul(perm_cost_per_gib_irys)
            .ok_or_eyre("Decimal operation failed")?;
        let final_fee = base_fee_irys
            .checked_mul(MINER_PERCENTAGE_FEE)
            .ok_or_eyre("Decimal operation failed")?;
        let scaled_fee = final_fee
            .checked_mul(SCALE_FACTOR)
            .ok_or_eyre("Decimal operation failed")?;
        u64::try_from(scaled_fee).map_err(|e| eyre::eyre!("Conversion failed for fee: {}", e))
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
    fn calc_perm_cost_per_gib(
        safe_minimum_number_of_years: Decimal,
        annualized_decay_rate: Decimal,
        partitions: Decimal,
        annualized_cost_of_storing_1gib: Decimal,
    ) -> eyre::Result<Decimal> {
        ensure!(
            safe_minimum_number_of_years != Decimal::ZERO,
            "Minimum number of years must be at least one"
        );
        ensure!(
            annualized_decay_rate > Decimal::ZERO,
            "Decay rate must be non-zero and positive"
        );

        let inner = Decimal::ONE
            .checked_sub(annualized_decay_rate)
            .ok_or_eyre("Decimal operation failed")?;
        let inner_exp = inner.powu(u64::try_from(safe_minimum_number_of_years)?);
        let dividend = Decimal::ONE
            .checked_sub(inner_exp)
            .ok_or_eyre("Decimal operation failed")?;
        let quotient = dividend
            .checked_div(annualized_decay_rate)
            .ok_or_eyre("Decimal operation failed")?;
        let total_cost = annualized_cost_of_storing_1gib
            .checked_mul(quotient)
            .ok_or_eyre("Decimal operation failed")?;
        total_cost
            .checked_mul(partitions)
            .ok_or_eyre("Decimal operation failed")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{storage_pricing::Amount, DecayParams, StorageFees};
    use k256::ecdsa::SigningKey;

    const EPSILON: Decimal = dec!(1e-9);

    fn get_expected_chunk_price() -> Option<Decimal> {
        // These values come from 200 years, 1% decay rate, n partitions, 5% miner fee
        const PRICE_FOR_1_PARTITION: Decimal = dec!(0.000002839035861659);
        const PRICE_FOR_10_PARTITIONS: Decimal = dec!(0.0006825861795615196);

        match get_config().num_partitions_per_slot {
            1 => Some(PRICE_FOR_1_PARTITION),
            10 => Some(PRICE_FOR_10_PARTITIONS),
            _ => None, // todo: if number of replicas, or partitions_per_slot are added, append them here
        }
    }

    fn get_annualized_cost_of_storing_1gib() -> Decimal {
        ANNUALIZED_COST_OF_OPERATING_16TB
            .checked_div(
                (dec!(16).checked_mul(Decimal::try_from(TEBIBYTE / GIBIBYTE).unwrap())).unwrap(),
            )
            .unwrap()
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
        let expected = 0;
        assert_eq!(expected, res)
    }

    #[test]
    fn test_calc_perm_storage_price_no_chunk() {
        let bytes_to_store = 0;
        let mut config = get_config();
        config.chunk_size = 0;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store, &config);
        assert!(res.is_err())
    }

    #[test]
    fn test_calc_perm_storage_price_256_bytes() {
        let bytes_to_store = 256;
        let config = get_config();
        let price = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let res = Decimal::from(price) / SCALE_FACTOR;
        let expected = get_expected_chunk_price().unwrap();
        assert_eq!(expected, res)
    }

    #[test]
    fn test_calc_perm_storage_price_1_chunk() {
        let config = get_config();
        let bytes_to_store = config.chunk_size;
        let price = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let res = Decimal::from(price) / SCALE_FACTOR;
        let expected = get_expected_chunk_price().unwrap();
        assert_eq!(expected, res)
    }

    #[test]
    fn test_calc_perm_storage_price_2_chunks() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 2;
        let price = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let res = Decimal::from(price) / SCALE_FACTOR;
        let expected = Decimal::from(2) * get_expected_chunk_price().unwrap();
        assert!((expected - res).abs() < EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_mib() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 4;
        let price = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let res = Decimal::from(price) / SCALE_FACTOR;
        let expected = dec!(4) * get_expected_chunk_price().unwrap();
        assert!((expected - res).abs() < EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_gib() {
        let config = get_config();
        let bytes_to_store = config.chunk_size * 4 * 1024;
        let price = PriceCalc::calc_perm_storage_price(bytes_to_store, &config).unwrap();
        let res = Decimal::from(price) / SCALE_FACTOR;
        let expected = dec!(4) * dec!(1024) * get_expected_chunk_price().unwrap();
        assert!((expected - res).abs() < EPSILON)
    }

    #[test]
    fn test_calc_perm_cost_per_gib_10_years() {
        let annualized_cost_of_storing_1gib = get_annualized_cost_of_storing_1gib();
        let safe_minimum_number_of_years = dec!(10);
        let annualized_decay_rate = dec!(0.01);
        let partitions = dec!(1);
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
            annualized_cost_of_storing_1gib,
        )
        .unwrap();
        let expected = dec!(0.025678641965);
        assert!((expected - res).abs() < EPSILON)
    }

    #[test]
    fn test_calc_perm_cost_per_gib_200_years() {
        let annualized_cost_of_storing_1gib = get_annualized_cost_of_storing_1gib();
        let safe_minimum_number_of_years = dec!(200);
        let annualized_decay_rate = dec!(0.01);
        let partitions = dec!(1);
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
            annualized_cost_of_storing_1gib,
        )
        .unwrap();
        let expected = dec!(0.23257381778);
        assert!((expected - res).abs() < EPSILON)
    }

    #[test]
    fn test_calc_perm_cost_per_gib_0_years() {
        let annualized_cost_of_storing_1gib = get_annualized_cost_of_storing_1gib();
        let safe_minimum_number_of_years = dec!(0);
        let annualized_decay_rate = dec!(0.01);
        let partitions = dec!(1);
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
            annualized_cost_of_storing_1gib,
        );
        assert_eq!(true, res.is_err())
    }

    #[test]
    fn test_calc_perm_cost_per_gib_0_decay_rate() {
        let annualized_cost_of_storing_1gib = get_annualized_cost_of_storing_1gib();
        let safe_minimum_number_of_years = dec!(200);
        let annualized_decay_rate = dec!(0.0);
        let partitions = dec!(1);
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
            annualized_cost_of_storing_1gib,
        );
        assert_eq!(true, res.is_err())
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
