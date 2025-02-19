use crate::{
    ANNUALIZED_COST_OF_OPERATING_16TB, ANNUALIZED_COST_OF_STORING_1GB, CONFIG, GIBIBYTE,
    MINER_PERCENTAGE_FEE, TB_PER_PARTITION, TEBIBYTE,
};
use eyre::{ensure, Error};
use rust_decimal::Decimal;

pub struct PriceCalc;

impl PriceCalc {
    fn get_usd_to_irys_conversion_rate() -> f64 {
        // 1 USD = how many $IRYS. end result is in $IRYS
        1.0
    }

    pub fn calc_perm_storage_price(number_of_bytes_to_store: u64) -> Result<f64, Error> {
        ensure!(CONFIG.chunk_size != 0, "Chunk size should not be 0");
        let perm_cost = Self::calc_perm_cost_per_gib(
            CONFIG.decay_params.safe_minimum_number_of_years,
            CONFIG.decay_params.annualized_decay_rate.try_into()?,
            CONFIG.num_partitions_per_slot,
        )?;
        let ingress_perm_fee = Self::calc_perm_fee_per_ingress_gib(
            CONFIG.storage_fees.number_of_ingress_proofs,
            CONFIG.storage_fees.ingress_fee,
        )?;
        let approximate_usd_irys_price = Self::get_usd_to_irys_conversion_rate();
        let chunks = Self::get_chunks_from_bytes(number_of_bytes_to_store);
        let chunks_per_gib = GIBIBYTE as u64 / CONFIG.chunk_size;
        let immediate_miner_reward = perm_cost * MINER_PERCENTAGE_FEE;
        Ok((chunks as f64 / chunks_per_gib as f64)
            * (ingress_perm_fee + perm_cost + immediate_miner_reward)
            * approximate_usd_irys_price)
    }

    fn get_chunks_from_bytes(number_of_bytes_to_store: u64) -> u64 {
        if number_of_bytes_to_store == 0 {
            return 0;
        }

        if number_of_bytes_to_store % CONFIG.chunk_size != 0 {
            number_of_bytes_to_store
                .checked_div(CONFIG.chunk_size)
                .expect("RHS is const")
                + 1
        } else {
            number_of_bytes_to_store
                .checked_div(CONFIG.chunk_size)
                .expect("RHS is const")
        }
    }

    fn calc_perm_cost_per_gib(
        safe_minimum_number_of_years: u32,
        annualized_decay_rate: f64,
        partitions: u64,
    ) -> Result<f64, Error> {
        let annualized_cost_of_operating_1_gib = ANNUALIZED_COST_OF_OPERATING_16TB
            / (TB_PER_PARTITION as f64 * (TEBIBYTE / GIBIBYTE) as f64);

        ensure!(
            safe_minimum_number_of_years != 0,
            "Minimum number of years must be at least one"
        );
        ensure!(
            annualized_decay_rate > 0.0,
            "Decay rate must be non-zero and positive"
        );

        let total_cost = annualized_cost_of_operating_1_gib
            * (1.0
                - f64::powi(
                    1.0 - annualized_decay_rate,
                    safe_minimum_number_of_years as i32,
                ))
            / annualized_decay_rate;
        Ok(total_cost * partitions as f64)
    }

    fn calc_perm_fee_per_ingress_gib(
        ingress_proofs: u32,
        ingress_fee: Decimal,
    ) -> Result<f64, Error> {
        ensure!(ingress_proofs != 0, "Ingress proofs must be > 0");
        let ingress_fee = f64::try_from(ingress_fee)?;
        Ok(ANNUALIZED_COST_OF_STORING_1GB + ingress_fee * ingress_proofs as f64)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use approx::assert_abs_diff_eq;

    const EPSILON: f64 = 1e-9;

    fn get_expected_chunk_price() -> Option<f64> {
        // These values come from 200 years, 1% decay rate, n partitions, 5% miner fee
        const PRICE_FOR_1_PARTITION: f64 = 8.674582693274584e-5;
        const PRICE_FOR_10_PARTITIONS: f64 = 0.0006233236047864428;

        match CONFIG.num_partitions_per_slot {
            1 => Some(PRICE_FOR_1_PARTITION),
            10 => Some(PRICE_FOR_10_PARTITIONS),
            _ => None, // todo: if number of replicas, or partitions_per_slot are added, append them here
        }
    }

    #[test]
    fn test_calc_perm_storage_price_0_bytes() {
        let bytes_to_store = 0;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = 0.0;
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_256_bytes() {
        let bytes_to_store = 256;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_chunk() {
        let bytes_to_store = CONFIG.chunk_size;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_2_chunks() {
        let bytes_to_store = CONFIG.chunk_size * 2;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = 2.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_mb() {
        let bytes_to_store = CONFIG.chunk_size * 4;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = 4.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_storage_price_1_gb() {
        let bytes_to_store = CONFIG.chunk_size * 4 * 1024;
        let res = PriceCalc::calc_perm_storage_price(bytes_to_store).unwrap();
        let expected = 4.0 * 1024.0 * get_expected_chunk_price().unwrap();
        assert_abs_diff_eq!(expected, res, epsilon = EPSILON)
    }

    #[test]
    fn test_calc_perm_cost_per_gb_10_years() {
        let safe_minimum_number_of_years = 10;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        )
        .unwrap();
        assert_abs_diff_eq!(0.0256786419, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_200_years() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gib(
            safe_minimum_number_of_years,
            annualized_decay_rate,
            partitions,
        )
        .unwrap();
        assert_abs_diff_eq!(0.23257381778716857, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_0_years() {
        let safe_minimum_number_of_years = 0;
        let annualized_decay_rate = 0.01;
        let partitions = 1;
        let res = PriceCalc::calc_perm_cost_per_gib(
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
        let res = PriceCalc::calc_perm_cost_per_gib(
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
        let res = PriceCalc::calc_perm_fee_per_ingress_gib(ingress_proofs, ingress_fee).unwrap();
        assert_abs_diff_eq!(0.11110839844, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_chunks() {
        const ONE_KB: u64 = 1024;

        let number_of_bytes_to_store = 0; // 0 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(0, res);
        let number_of_bytes_to_store = 1; // 1 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(1, res);
        let number_of_bytes_to_store = 256 * ONE_KB; // 256 KiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(1, res);
        let number_of_bytes_to_store = 257 * ONE_KB;
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(2, res);
        let number_of_bytes_to_store = 1_024 * ONE_KB; // 1 MiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4, res);
        let number_of_bytes_to_store = 1_025 * ONE_KB;
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(5, res);
        let number_of_bytes_to_store = 1_073_741_824; // 1 GiB
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4096, res);
        let number_of_bytes_to_store = 1_073_741_825; // 1 GiB + 1 B
        let res = PriceCalc::get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4097, res);
    }
}
