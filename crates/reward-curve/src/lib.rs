//! Exponential-decay (base-2) emission curve
//!
//! R(t) = R_max · ln 2 / T½ · 2^-(t/T½)
//!
//! All arithmetic is 18-decimal fixed-point using 256-bit unsigned integers.

use eyre::{eyre, Result};
use irys_types::storage_pricing::{
    mul_div,
    safe_add,
    safe_div,
    safe_sub,
    TOKEN_SCALE, // TOKEN_SCALE = 1 e18
};
use irys_types::storage_pricing::{phantoms::Irys, Amount};
use irys_types::U256;

/// ln 2 in 18-dec fixed-point:
/// 0.693 147 180 559 945 309 × 1 e18 ≈ 693 147 180 559 945 309
const LN2_FP18: U256 = U256([693_147_180_559_945_309u64, 0, 0, 0]);

/// Continuous halving emission curve.
///
/// * **inflation_cap** – asymptotic total emission, atomic units (R_max)  
/// * **half_life_secs** – seconds until the remaining emission halves (T½)
#[derive(Debug, Clone)]
pub struct HalvingCurve {
    pub inflation_cap: Amount<Irys>,
    pub half_life_secs: u128,
}

impl HalvingCurve {
    /// Block reward at the given timestamp (seconds since genesis).
    pub fn block_reward(&self, seconds_since_genesis_block: u128) -> Result<Amount<Irys>> {
        if self.inflation_cap.amount.is_zero() || self.half_life_secs == 0 {
            return Ok(Amount::new(U256::zero()));
        }

        // r_ln2 = R_max · ln 2 / T½  (18-dec)
        let r_ln2_fp18 = mul_div(
            self.inflation_cap.amount,
            LN2_FP18,
            U256::from(self.half_life_secs),
        )?;

        // decay = 2^-(t/T½)  (18-dec)
        let decay_fp18 = decay_factor(seconds_since_genesis_block, self.half_life_secs)?;

        // reward = r_ln2 · decay / 1 e18  ⟶ atomic units
        let reward = mul_div(r_ln2_fp18, decay_fp18, TOKEN_SCALE)?;
        Ok(Amount::new(reward))
    }

    /// Reward for the time span *(prev_ts … new_ts]*  (both in seconds since genesis).
    ///
    /// Guarantees `Ok(0)` when the interval is empty and
    /// `Err(..)` if `new_ts < prev_ts`.
    pub fn reward_between(&self, prev_ts: u128, new_ts: u128) -> Result<Amount<Irys>> {
        if new_ts < prev_ts {
            return Err(eyre!("new_ts ({new_ts}) < prev_ts ({prev_ts})"));
        }
        if prev_ts == new_ts || self.inflation_cap.amount.is_zero() {
            return Ok(Amount::new(U256::zero()));
        }

        // Cumulative emission up to each endpoint
        let emitted_prev = self.emitted_until(prev_ts)?;
        let emitted_new = self.emitted_until(new_ts)?;

        // Δ-emission is the block reward
        let delta = safe_sub(emitted_new, emitted_prev)?;
        Ok(Amount::new(delta))
    }

    /// Tokens emitted from genesis **up to** `t` (seconds).
    fn emitted_until(&self, t: u128) -> Result<U256> {
        // decay = 2^-(t/T½)  (18-dec)
        let decay_fp18 = decay_factor(t, self.half_life_secs)?;

        // emitted = R_max · (1 − decay)
        let one_minus = safe_sub(TOKEN_SCALE, decay_fp18)?;
        mul_div(self.inflation_cap.amount, one_minus, TOKEN_SCALE)
    }
}

/// 2^-(t / half_life) in 18-dec fixed-point.
///
/// Split `t / half_life = q + f` into integer `q` and fractional `f`.
/// * 2^-q ⇒ divide by 2^q (bit-shift)  
/// * 2^-f ⇒ e^(-ln 2 · f) via truncated Taylor series
fn decay_factor(t_secs: u128, half_life: u128) -> Result<U256> {
    if half_life == 0 {
        return Err(eyre!("half_life cannot be zero"));
    }

    let q = t_secs / half_life;
    let f_secs = t_secs % half_life;

    // 2^-q
    let decay_q_fp18 = if q == 0 {
        TOKEN_SCALE
    } else if q >= 256 {
        U256::zero() // underflow
    } else {
        safe_div(TOKEN_SCALE, U256::one() << q)?
    };

    // No fractional part? Done.
    if f_secs == 0 || decay_q_fp18.is_zero() {
        return Ok(decay_q_fp18);
    }

    // x = ln 2 · f / half_life  (18-dec)
    let x_fp18 = mul_div(LN2_FP18, U256::from(f_secs), U256::from(half_life))?;
    let decay_f_fp18 = exp_neg(x_fp18)?;

    // Combine integer and fractional pieces.
    mul_div(decay_q_fp18, decay_f_fp18, TOKEN_SCALE)
}

/// e^(-x) in 18-dec fixed-point using `TAYLOR_TERMS` terms.
fn exp_neg(x_fp18: U256) -> Result<U256> {
    /// Number of terms kept in the e^(-x) Taylor series.
    /// 10 → |ε| < 3 × 10⁻¹⁵.
    const TAYLOR_TERMS: u128 = 10;

    let mut term = TOKEN_SCALE; // current term (= 1)
    let mut sum = TOKEN_SCALE; // running total

    for i in 1..=TAYLOR_TERMS {
        term = mul_div(term, x_fp18, TOKEN_SCALE)?; // term *= x
        term = safe_div(term, U256::from(i))?; // term /= i
        sum = if i & 1 == 1 {
            // alternating signs
            safe_sub(sum, term)?
        } else {
            safe_add(sum, term)?
        };
    }
    Ok(sum)
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
    /// S(t) = R_max · (1 − 2^{-t/T½})
    fn circulating_supply(curve: &HalvingCurve, t_years: u128) -> Result<u128> {
        use irys_types::storage_pricing::{mul_div, safe_sub, TOKEN_SCALE};

        let elapsed_secs = t_years * SECS_PER_YEAR;

        // decay = 2^{-t/T½} (scaled 1e18)
        let decay_scaled = decay_factor(elapsed_secs, curve.half_life_secs)?;

        // emitted = R_max · (1 − decay)
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

        assert!(
            (actual.as_u128() as i128 - expected as i128).abs() <= 1,
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

    /// new_ts < prev_ts => error.
    #[test]
    fn reward_between_invalid_interval_errors() {
        let curve = test_curve();
        let res = curve.reward_between(secs(10), secs(9)); // reversed
        assert!(res.is_err(), "expected error for reversed interval");
    }
}
