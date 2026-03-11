//! Exponential-decay (base-2) emission curve
//!
//! R(t) = R_max * ln(2) / T_half * 2^-(t / T_half)
//!
//! All arithmetic is 18-decimal fixed-point using 256-bit unsigned integers.

use eyre::{Result, eyre};
use irys_types::U256;
use irys_types::storage_pricing::{Amount, LN2_FP18, exp_neg_fp18, phantoms::Irys};
use irys_types::storage_pricing::{TOKEN_SCALE, mul_div, safe_div, safe_sub};

/// Continuous halving emission curve
///
/// - inflation_cap: maximum total emission, in atomic units (R_max)
/// - half_life_secs: time in seconds until remaining emission is halved (T_half)
#[derive(Debug, Clone)]
pub struct HalvingCurve {
    pub inflation_cap: Amount<Irys>,
    pub half_life_secs: u128,
}

impl HalvingCurve {
    /// Returns the emitted reward for the time interval (prev_ts ..= new_ts)
    ///
    /// Returns Ok(0) if the interval is empty.
    /// Returns Err if new_ts is earlier than prev_ts.
    pub fn reward_between(&self, prev_ts: u128, new_ts: u128) -> Result<Amount<Irys>> {
        if new_ts < prev_ts {
            return Err(eyre!("new_ts ({new_ts}) < prev_ts ({prev_ts})"));
        }
        if prev_ts == new_ts || self.inflation_cap.amount.is_zero() {
            return Ok(Amount::new(U256::zero()));
        }

        // Compute total emission up to each timestamp
        let emitted_prev = self.emitted_until(prev_ts)?;
        let emitted_new = self.emitted_until(new_ts)?;

        // Difference in emission is the reward
        let delta = safe_sub(emitted_new, emitted_prev)?;
        Ok(Amount::new(delta))
    }

    /// Returns total tokens emitted from genesis up to time t (in seconds)
    fn emitted_until(&self, t: u128) -> Result<U256> {
        // decay = 2^-(t / T_half) in fixed-point
        let decay_fp18 = decay_factor(t, self.half_life_secs)?;

        // emitted = R_max * (1 - decay)
        let one_minus = safe_sub(TOKEN_SCALE, decay_fp18)?;
        mul_div(self.inflation_cap.amount, one_minus, TOKEN_SCALE)
    }
}

/// Computes 2^-(t / half_life) in 18-decimal fixed-point
///
/// Splits t / half_life into integer part q and fractional part f
/// - 2^-q is computed by bit-shifting
/// - 2^-f is approximated using a truncated Taylor expansion of exp(-ln(2) * f)
fn decay_factor(t_secs: u128, half_life: u128) -> Result<U256> {
    if half_life == 0 {
        return Err(eyre!("half_life cannot be zero"));
    }

    let q = t_secs / half_life;
    let f_secs = t_secs % half_life;

    // Compute 2^-q
    let decay_q_fp18 = if q == 0 {
        TOKEN_SCALE
    // Since TOKEN_SCALE = 1e18 and 2^60 ≈ 1.15e18, TOKEN_SCALE / 2^q == 0 for q >= 60
    } else if q >= 60 {
        U256::zero() // result underflows to zero
    } else {
        safe_div(TOKEN_SCALE, U256::one() << q)?
    };

    // If no fractional remainder, we are done
    if f_secs == 0 || decay_q_fp18.is_zero() {
        return Ok(decay_q_fp18);
    }

    // Compute ln(2) * f / half_life in fixed-point
    let x_fp18 = mul_div(LN2_FP18, U256::from(f_secs), U256::from(half_life))?;
    let decay_f_fp18 = exp_neg_fp18(x_fp18)?;

    // Final decay = 2^-q * 2^-f
    mul_div(decay_q_fp18, decay_f_fp18, TOKEN_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    use eyre::Result;
    use rstest::rstest;

    const INF_SUPPLY: u128 = 100_000_000; // R_max (tokens)
    const HALF_LIFE_YEARS: u128 = 4; // half-life
    const SECS_PER_YEAR: u128 = 365 * 24 * 60 * 60; // 31 536 000

    /// Cumulative emitted tokens after `t_years`, computed
    /// **with the same integer math** that `LogCurve::miner_reward` uses:
    ///
    /// S(t) = R_max * (1 − 2^{-t/T_half})
    fn circulating_supply(curve: &HalvingCurve, t_years: u128) -> Result<u128> {
        use irys_types::storage_pricing::{TOKEN_SCALE, mul_div, safe_sub};

        let elapsed_secs = t_years * SECS_PER_YEAR;

        // decay = 2^{-t/T_half} (scaled 1e18)
        let decay_scaled = decay_factor(elapsed_secs, curve.half_life_secs)?;

        // emitted = R_max * (1 − decay)
        let one_minus = safe_sub(TOKEN_SCALE, decay_scaled)?;
        let emitted = mul_div(curve.inflation_cap.amount, one_minus, TOKEN_SCALE)?;

        // numbers are tiny (≤ 1e8) → always fit in u128
        Ok(<u128>::try_from(emitted).expect("fits into u128"))
    }

    fn test_curve() -> HalvingCurve {
        HalvingCurve {
            inflation_cap: Amount::new(U256::from(INF_SUPPLY)),
            half_life_secs: HALF_LIFE_YEARS * SECS_PER_YEAR,
        }
    }

    /// Convenience: convert years -> seconds since genesis.
    fn secs(years: u128) -> u128 {
        years * SECS_PER_YEAR
    }

    /// Δ-supply between two years, via the same integer math.
    fn expected_reward(curve: &HalvingCurve, from_year: u128, to_year: u128) -> Result<u128> {
        let s0 = circulating_supply(curve, from_year)?;
        let s1 = circulating_supply(curve, to_year)?;
        Ok(s1 - s0)
    }

    // table-driven assertions
    #[rstest]
    #[case(0, 0)]
    #[case(1, 15_910_358)]
    #[case(2, 29_289_322)]
    #[case(3, 40_539_644)]
    #[case(4, 50_000_000)]
    #[case(5, 57_955_179)]
    #[case(6, 64_644_661)]
    #[case(7, 70_269_822)]
    #[case(8, 75_000_000)]
    #[case(9, 78_977_590)]
    #[case(10, 82_322_330)]
    #[case(11, 85_134_911)]
    #[case(12, 87_500_000)]
    #[case(13, 89_488_795)]
    #[case(14, 91_161_165)]
    #[case(15, 92_567_456)]
    #[case(16, 93_750_000)]
    #[case(17, 94_744_397)]
    #[case(18, 95_580_583)]
    #[case(19, 96_283_728)]
    fn circulating_supply_matches_table(#[case] year: u128, #[case] expected: u128) -> Result<()> {
        let curve = test_curve();
        let actual = circulating_supply(&curve, year)?;

        // there's a potential rounding error of a single token between the source impl and the test data (taken from excel)
        // - the Excel sheet rounded to nearest integer (source of the result)
        // - our helper truncates (floor-divides) after the final mul_div.
        assert!(
            (actual as i128 - expected as i128).abs() <= 1,
            "year {year}: expected {expected}, got {actual}"
        );
        Ok(())
    }

    /// Golden-path table: reward for each [y, y+1) interval.
    #[rstest]
    #[case(0, 1)]
    #[case(1, 2)]
    #[case(3, 4)]
    #[case(7, 8)]
    #[case(18, 19)]
    fn reward_between_matches_delta_supply(
        #[case] from_year: u128,
        #[case] to_year: u128,
    ) -> Result<()> {
        let curve = test_curve();

        // what the curve says
        let actual = curve.reward_between(secs(from_year), secs(to_year))?.amount;

        // what the integral says
        let expected = expected_reward(&curve, from_year, to_year)?;

        assert_eq!(
            actual,
            U256::from(expected),
            "Δ[{from_year},{to_year}]: expected {expected}, got {actual}"
        );
        Ok(())
    }

    /// Golden-path table: reward for each [y, y+1) interval.
    #[rstest]
    #[case(0, 1)]
    #[case(1, 2)]
    #[case(3, 4)]
    #[case(7, 8)]
    #[case(18, 19)]
    fn sanity_check_with_18_decimals(#[case] from_year: u128, #[case] to_year: u128) -> Result<()> {
        let curve = HalvingCurve {
            // this will generate a max cap with 18 decimals
            inflation_cap: Amount::token(INF_SUPPLY.into()).unwrap(),
            half_life_secs: HALF_LIFE_YEARS * SECS_PER_YEAR,
        };

        // what the curve says
        let actual = curve.reward_between(secs(from_year), secs(to_year))?.amount;

        // what the integral says
        let expected = expected_reward(&curve, from_year, to_year)?;

        assert_eq!(
            actual,
            U256::from(expected),
            "Δ[{from_year},{to_year}]: expected {expected}, got {actual}"
        );
        Ok(())
    }

    /// Empty interval => zero reward.
    #[test]
    fn reward_between_zero_interval_is_zero() -> Result<()> {
        let curve = test_curve();
        let ts = secs(5); // arbitrary
        let reward = curve.reward_between(ts, ts)?;
        assert!(reward.amount.is_zero());
        Ok(())
    }

    #[test]
    fn decay_factor_q_cutoff_behavior() -> Result<()> {
        // With half_life = 1, q = t/half_life = t
        // q = 59 -> non-zero; q >= 60 -> zero (since TOKEN_SCALE / 2^q truncates to 0)
        let non_zero = decay_factor(59, 1)?;
        assert!(non_zero > U256::zero(), "q=59 should be non-zero");

        let zero_60 = decay_factor(60, 1)?;
        assert!(zero_60.is_zero(), "q=60 should be zero");

        let zero_100 = decay_factor(100, 1)?;
        assert!(zero_100.is_zero(), "q=100 should be zero");

        Ok(())
    }

    /// new_ts < prev_ts => error.
    #[test]
    fn reward_between_invalid_interval_errors() {
        let curve = test_curve();
        let res = curve.reward_between(secs(10), secs(9)); // reversed
        assert!(res.is_err(), "expected error for reversed interval");
    }

    mod prop_halving_curve {
        use super::*;
        use proptest::prelude::*;

        /// eyre::Report does not implement std::error::Error, so proptest's `?`
        /// cannot convert it to TestCaseError automatically.
        fn eyre_to_prop<T>(r: eyre::Result<T>) -> std::result::Result<T, TestCaseError> {
            r.map_err(|e| TestCaseError::fail(format!("{e:#}")))
        }

        fn arb_curve() -> impl Strategy<Value = HalvingCurve> {
            (1_u128..1_000_000_000_000_000_000, 1_u128..3_153_600_000).prop_map(
                |(cap, half_life)| HalvingCurve {
                    inflation_cap: Amount::new(U256::from(cap)),
                    half_life_secs: half_life,
                },
            )
        }

        fn arb_timestamp() -> impl Strategy<Value = u128> {
            0_u128..6_307_200_000
        }

        proptest! {
            #[test]
            fn prop_emission_bounded_by_cap(
                curve in arb_curve(),
                t in arb_timestamp(),
            ) {
                let emitted = eyre_to_prop(curve.emitted_until(t))?;
                prop_assert!(
                    emitted <= curve.inflation_cap.amount,
                    "emitted {} exceeded cap {} at t={}",
                    emitted, curve.inflation_cap.amount, t
                );
            }

            #[test]
            fn prop_reward_additivity(
                curve in arb_curve(),
                t0 in arb_timestamp(),
                delta_ab in 0_u128..315_360_000,
                delta_bc in 0_u128..315_360_000,
            ) {
                let a = t0;
                let b = a.saturating_add(delta_ab);
                let c = b.saturating_add(delta_bc);

                let r_ab = eyre_to_prop(curve.reward_between(a, b))?.amount;
                let r_bc = eyre_to_prop(curve.reward_between(b, c))?.amount;
                let r_ac = eyre_to_prop(curve.reward_between(a, c))?.amount;

                let sum = r_ab.checked_add(r_bc).expect("sum should not overflow U256");

                // Two intermediate truncations can accumulate up to 2 units of rounding error
                let diff = if sum >= r_ac { sum - r_ac } else { r_ac - sum };
                prop_assert!(
                    diff <= U256::from(2_u64),
                    "additivity violated: r(a,b)={} + r(b,c)={} = {}, but r(a,c)={}, diff={}",
                    r_ab, r_bc, sum, r_ac, diff
                );
            }

            #[test]
            fn prop_emission_monotonically_increases(
                curve in arb_curve(),
                t1 in arb_timestamp(),
                delta in 0_u128..3_153_600_000,
            ) {
                let t2 = t1.saturating_add(delta);
                let e1 = eyre_to_prop(curve.emitted_until(t1))?;
                let e2 = eyre_to_prop(curve.emitted_until(t2))?;
                prop_assert!(
                    e1 <= e2,
                    "monotonicity violated: emitted_until({})={} > emitted_until({})={}",
                    t1, e1, t2, e2
                );
            }
        }
    }

    #[test]
    fn test_zero_half_life_errors() {
        let curve = HalvingCurve {
            inflation_cap: Amount::new(U256::from(1_000_000_u64)),
            half_life_secs: 0,
        };
        let result = curve.reward_between(0, 100);
        assert!(result.is_err());

        let decay_result = decay_factor(100, 0);
        assert!(decay_result.is_err());
    }

    #[test]
    fn decay_factor_fractional_remainder_reduces_factor() -> Result<()> {
        // t=5, half_life=2 → q=2, f_secs=1: non-zero and fractional part matters
        let result = decay_factor(5, 2)?;
        assert!(
            result > U256::zero(),
            "q=2 with fractional part should be non-zero"
        );

        let result_no_frac = decay_factor(4, 2)?;
        assert!(
            result < result_no_frac,
            "adding fractional decay should reduce the factor"
        );
        Ok(())
    }

    #[test]
    fn test_zero_cap_emits_nothing() -> Result<()> {
        let curve = HalvingCurve {
            inflation_cap: Amount::new(U256::zero()),
            half_life_secs: HALF_LIFE_YEARS * SECS_PER_YEAR,
        };

        let reward = curve.reward_between(0, secs(10))?.amount;
        assert!(
            reward.is_zero(),
            "expected zero emission with zero cap, got {reward}"
        );

        let reward_large = curve.reward_between(0, secs(1000))?.amount;
        assert!(
            reward_large.is_zero(),
            "expected zero emission at t=1000 years with zero cap, got {reward_large}"
        );

        Ok(())
    }
}
