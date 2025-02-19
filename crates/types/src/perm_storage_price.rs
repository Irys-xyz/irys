use crate::{ANNUALIZED_COST_OF_STORING_1GB, CONFIG};
use eyre::{ensure, Error};

pub struct PriceCalc;

impl PriceCalc {
    const CHUNKS_PER_GB: u32 = 4 * 1024; // 256 KiB chunk size
    const GB_PER_TB: u32 = 1024;
    const TB_PER_DRIVE: u32 = 16;

    fn get_usd_to_irys_conversion_rate() -> f64 {
        // 1 USD = how many $IRYS. end result is in $IRYS
        1.0
    }

    pub fn calc_perm_storage_price(number_of_bytes_to_store: u64) -> Result<f64, Error> {
        let perm_cost = Self::calc_perm_cost_per_gb(
            CONFIG.decay_params.safe_minimum_number_of_years,
            CONFIG.decay_params.annualized_decay_rate.try_into()?,
        )?;
        let perm_fee = Self::calc_perm_fee_per_gb(perm_cost)?;
        let approximate_usd_irys_price = Self::get_usd_to_irys_conversion_rate();
        let chunks = Self::get_chunks_from_bytes(number_of_bytes_to_store);
        Ok(chunks as f64 * perm_fee * approximate_usd_irys_price / Self::CHUNKS_PER_GB as f64)
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

    fn calc_perm_cost_per_gb(
        safe_minimum_number_of_years: u32,
        annualized_decay_rate: f64,
    ) -> Result<f64, Error> {
        const ANNUALIZED_COST_OF_STORING_16TB: f64 = 44.0;
        let annualized_cost_of_storing_1_tb =
            ANNUALIZED_COST_OF_STORING_16TB / Self::TB_PER_DRIVE as f64;

        ensure!(
            safe_minimum_number_of_years != 0,
            "Minimum number of years must be at least one"
        );
        ensure!(
            annualized_decay_rate > 0.0,
            "Decay rate must be non-zero and positive"
        );

        let total_cost = (annualized_cost_of_storing_1_tb / Self::GB_PER_TB as f64)
            * (1.0
                - f64::powi(
                    1.0 - annualized_decay_rate,
                    safe_minimum_number_of_years as i32,
                ))
            / annualized_decay_rate;
        Ok(total_cost * CONFIG.num_partitions_per_slot as f64)
    }

    fn calc_perm_fee_per_gb(perm_cost: f64) -> Result<f64, Error> {
        let ingress_proofs = CONFIG.storage_fees.number_of_ingress_proofs;
        ensure!(ingress_proofs != 0, "Ingress proofs must be > 0");
        let ingress_fee = f64::try_from(CONFIG.storage_fees.ingress_fee)?;
        Ok(ANNUALIZED_COST_OF_STORING_1GB + (ingress_fee * ingress_proofs as f64) + perm_cost)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use approx::assert_abs_diff_eq;

    const EPSILON: f64 = 1e-9;

    #[test]
    fn test_calc_perm_cost_per_gb_10_years() {
        let safe_minimum_number_of_years = 10;
        let annualized_decay_rate = 0.01;
        let res =
            PriceCalc::calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate)
                .unwrap();
        assert_abs_diff_eq!(0.0256786419, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_200_years() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.01;
        let res =
            PriceCalc::calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate)
                .unwrap();
        assert_abs_diff_eq!(0.23257381778716857, res, epsilon = EPSILON);
    }

    #[test]
    fn test_calc_perm_cost_per_gb_0_years() {
        let safe_minimum_number_of_years = 0;
        let annualized_decay_rate = 0.01;
        if PriceCalc::calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate)
            .is_ok()
        {
            assert!(true)
        }
    }

    #[test]
    fn test_calc_perm_cost_per_gb_0_decay_rate() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.0;
        if PriceCalc::calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate)
            .is_ok()
        {
            assert!(true)
        }
    }

    #[test]
    fn test_get_perm_fee() {
        let res = PriceCalc::calc_perm_fee_per_gb(2.33).unwrap();
        assert_abs_diff_eq!(2.44110839844, res, epsilon = EPSILON);
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
