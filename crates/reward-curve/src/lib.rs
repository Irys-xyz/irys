use eyre::{eyre, Result};
use irys_types::storage_pricing::{mul_div, safe_add, safe_div, safe_sub, TOKEN_SCALE};
use irys_types::storage_pricing::{phantoms::Irys, Amount};
use irys_types::U256;

/// 18-decimal fixed-point representation of ln(2).
/// 0.693147180559945309 × 10¹⁸  ≈  693_147_180_559_945_309
const LN2_SCALED: U256 = U256([693_147_180_559_945_309u64, 0, 0, 0]);

/// Exponential–decay emission curve:
///
/// R(t) = R_max · ln(2)/T½ · 2⁻(t/T½)
///
/// Everything is integer / fixed-point.  
/// `inflation_supply` – asymptotic total emission (atomic units)  
/// `half_life_secs`   – half-life in **seconds**
#[derive(Debug, Clone)]
pub struct LogCurve {
    pub inflation_supply: Amount<Irys>,
    pub half_life_secs: u64,
}

impl LogCurve {
    /// Per-block reward at `block_timestamp` (seconds since genesis).
    ///
    /// Returns `Amount<Irys>` in atomic units.
    pub fn miner_reward(&self, block_timestamp: u64) -> Result<Amount<Irys>> {
        if self.inflation_supply.amount.is_zero() || self.half_life_secs == 0 {
            return Ok(Amount::new(U256::zero()));
        }

        //--------------------------------------------------------------------
        // 1.  R_max · ln(2) / T½   (scaled 1e18)
        //--------------------------------------------------------------------
        let r_ln2_scaled = mul_div(
            self.inflation_supply.amount,
            LN2_SCALED,
            U256::from(self.half_life_secs),
        )?;

        //--------------------------------------------------------------------
        // 2.  decay = 2^-(t / T½)  (scaled 1e18)
        //--------------------------------------------------------------------
        let decay_scaled = pow_half_ratio(block_timestamp, self.half_life_secs)?;

        //--------------------------------------------------------------------
        // 3.  reward = (r_ln2_scaled · decay_scaled) / 1e18   → atomic units
        //--------------------------------------------------------------------
        let reward = mul_div(r_ln2_scaled, decay_scaled, TOKEN_SCALE)?;

        Ok(Amount::new(reward))
    }
}

/*──────────────────────── helper functions ──────────────────────────*/

/// 2^-(t / half_life)  in 1e18 fixed-point.
///
/// Split t / T½  into its integer part  *q*  and fractional part  *f*.
/// * 2⁻ᵠ  is just a right-shift / division by 2ᵠ.
/// * 2⁻ᶠ = e^(-ln2·f)  is approximated with a 6-term Taylor series;
///   f ≤ 1 so the error stays < 10⁻¹² for practical ranges.
fn pow_half_ratio(timestamp: u64, half_life: u64) -> Result<U256> {
    if half_life == 0 {
        return Err(eyre!("half_life cannot be zero"));
    }

    let whole = timestamp / half_life;
    let remainder = timestamp % half_life;

    // TOKEN_SCALE / 2^whole
    let mut result = TOKEN_SCALE;
    if whole > 0 {
        // shift >255 would underflow straight to zero – early-out
        if whole >= 256 {
            return Ok(U256::zero());
        }
        let pow2_whole = U256::one() << whole;
        result = safe_div(result, pow2_whole)?;
    }

    // If there is no fractional part we are done.
    if remainder == 0 {
        return Ok(result);
    }

    // x = ln2 * remainder / half_life   (scaled 1e18)
    let x_scaled = mul_div(LN2_SCALED, U256::from(remainder), U256::from(half_life))?;
    let exp_frac = exp_neg_fixed(x_scaled)?;

    // combine: 2^-whole * 2^-fraction
    mul_div(result, exp_frac, TOKEN_SCALE)
}

/// e^(-x) in 18-dec fixed-point for 0 ≤ x ≤ ~0.7 (covers ln2*fraction).
/// 6-term alternating Taylor series  ⇒  relative error < 10⁻¹².
fn exp_neg_fixed(x_scaled: U256) -> Result<U256> {
    let mut term = TOKEN_SCALE; // current term (starts at 1)
    let mut sum = TOKEN_SCALE; // running total

    for i in 1..=6u64 {
        term = mul_div(term, x_scaled, TOKEN_SCALE)?; // term *= x
        term = safe_div(term, U256::from(i))?; // term /= i
        if i & 1 == 1 {
            // alternate signs
            sum = safe_sub(sum, term)?;
        } else {
            sum = safe_add(sum, term)?;
        }
    }
    Ok(sum)
}
