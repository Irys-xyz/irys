//! A utility module for calculating network fees, costs for storing different amounts of data, and EMA for blocks.
//!
//! Core data types:
//! - `Amount<(CostPerChunk, Usd)>` - Cost in $USD of storing 1 chunk on irys (per single replica), data part of the config
//! - `Amount<(IrysPrice, Usd)>` - Cost in $USD of a single $IRYS token, the data retrieved form oracles
//! - `Amount<(NetworkFee, Irys)>` - The cost in $IRYS that the user will have to pay to store his data on Irys

pub use crate::U256;
use alloy_rlp::{Decodable, Encodable};
use arbitrary::Arbitrary;
use core::{fmt::Debug, marker::PhantomData};
use eyre::{ensure, eyre, Result};
use reth_codecs::Compact;
pub use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 1.0 in 18-decimal fixed point. little endian encoded.
/// Used by token price representations.
pub const TOKEN_SCALE: U256 = U256([TOKEN_SCALE_NATIVE, 0, 0, 0]);
const TOKEN_SCALE_NATIVE: u64 = 1_000_000_000_000_000_000_u64;

/// High precision scale for percentage representations.
/// 100% = 1_000_000_000_000 (1e12) for better precision with very small values.
/// This allows us to accurately represent decay rates as small as 0.000001%.
pub const PRECISION_SCALE: U256 = U256([PRECISION_SCALE_NATIVE, 0, 0, 0]);
const PRECISION_SCALE_NATIVE: u64 = 1_000_000_000_000;

/// ln(2) in 18-decimal fixed-point:
pub const LN2_FP18: U256 = U256([693_147_180_559_945_309_u64, 0, 0, 0]);
const TAYLOR_TERMS: u32 = 30;

pub const TERM_FEE: U256 = U256([1_000_000_000, 0, 0, 0]);

/// `Amount<T>` represents a value stored as a U256.
///
/// The actual scale is defined by the usage: pr
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary, Default,
)]
pub struct Amount<T> {
    pub amount: U256,
    #[serde(skip_serializing, default)]
    _t: PhantomData<T>,
}

impl<T> Encodable for Amount<T> {
    #[inline]
    fn length(&self) -> usize {
        self.amount.length()
    }

    #[inline]
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        self.amount.encode(out);
    }
}

impl<T: std::fmt::Debug> Decodable for Amount<T> {
    #[tracing::instrument(err)]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let res = U256::decode(buf)?;
        Ok(Self::new(res))
    }
}

impl<T> Compact for Amount<T> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.amount.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (instance, buf) = U256::from_compact(buf, len);
        (
            Self {
                amount: instance,
                _t: PhantomData,
            },
            buf,
        )
    }
}

impl<T: std::fmt::Debug> Amount<T> {
    #[must_use]
    pub const fn new(amount: U256) -> Self {
        Self {
            amount,
            _t: PhantomData,
        }
    }

    #[tracing::instrument(err)]
    pub fn token(amount: Decimal) -> Result<Self> {
        let amount = Self::decimal_to_u256(amount, TOKEN_SCALE_NATIVE)?;
        Ok(Self::new(amount))
    }

    #[tracing::instrument(err)]
    pub fn percentage(amount: Decimal) -> Result<Self> {
        let amount = Self::decimal_to_u256(amount, PRECISION_SCALE_NATIVE)?;
        Ok(Self::new(amount))
    }

    /// Helper to convert a Decimal into a scaled U256.
    #[tracing::instrument(err)]
    pub fn decimal_to_u256(dec: Decimal, scale: u64) -> Result<U256> {
        // Only handle non-negative decimals.
        ensure!(dec >= Decimal::ZERO, "decimal must be non-negative");

        // Get the underlying integer representation and the scale.
        // A Decimal represents: value = mantissa / 10^(dec.scale())
        let unscaled = dec.mantissa().unsigned_abs();
        let dec_scale = dec.scale();

        // divisor = 10^(dec.scale())
        let divisor = U256::from(
            10_u128
                .checked_pow(dec_scale)
                .ok_or_else(|| eyre::eyre!("decimal scale too large 10^{dec_scale}"))?,
        );

        // For rounding, add half the divisor.
        let half_divisor = safe_div(divisor, U256::from(2_u8))?;

        // Multiply the unscaled value by the target scale in U256 arithmetic.
        let numerator = safe_mul(U256::from(unscaled), U256::from(scale))?;

        // Perform the division with rounding.
        let res = safe_div(safe_add(numerator, half_divisor)?, divisor)?;
        Ok(res)
    }

    /// Helper to convert a U256 (with 18 decimals) into a `Decimal` for assertions.
    /// Assumes that the U256 value is small enough to fit into a u128.
    #[tracing::instrument(err)]
    pub fn token_to_decimal(&self) -> Result<Decimal> {
        self.amount_to_decimal(TOKEN_SCALE, TOKEN_SCALE_NATIVE)
    }

    /// Helper to convert a U256 (with 6 decimals) into a `Decimal` for assertions.
    /// Assumes that the U256 value is small enough to fit into a u128.
    #[tracing::instrument(err)]
    pub fn percentage_to_decimal(&self) -> Result<Decimal> {
        self.amount_to_decimal(PRECISION_SCALE, PRECISION_SCALE_NATIVE)
    }

    #[tracing::instrument(err)]
    fn amount_to_decimal(&self, scale: U256, scale_native: u64) -> Result<Decimal> {
        // Compute the integer and fractional parts.
        let quotient = safe_div(self.amount, scale)?;
        let remainder = safe_mod(self.amount, scale)?;

        // Convert quotient and remainder to u128.
        let quotient: u128 = u128::try_from(quotient).expect("quotient fits in u128");
        let remainder: u128 = u128::try_from(remainder).expect("remainder fits in u128");

        // Build the Decimal value:
        // The quotient represents the integer part,
        // while the remainder scaled by 1e-`scale_native` is the fractional part.
        let remainder = Decimal::from(remainder)
            .checked_div(Decimal::from(scale_native))
            .ok_or_else(|| {
                eyre::eyre!(
                    "scaling back remainder {remainder} decimal from 1e-18 cannot be computed"
                )
            })?;
        let res = Decimal::from(quotient)
            .checked_add(remainder)
            .ok_or_else(|| {
                eyre::eyre!("decimal overflow: quotient={quotient} remainder={remainder}")
            })?;
        Ok(res)
    }
}

// Phantom markers for type safety.
pub mod phantoms {
    use arbitrary::Arbitrary;

    /// The cost of storing a single chunk of data.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct CostPerChunk;

    /// Currency denominator util type.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct Usd;

    /// Currency denominator util type.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct Irys;

    /// Decay rate to account for storage hardware getting cheaper.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct DecayRate;

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct Percentage;

    /// The network fee, that the user would have to pay for storing his data on Irys.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct NetworkFee;

    /// Price of the $IRYS token.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct IrysPrice;

    /// Cost per storing 1 chunk of data. Includes adjustment for storage duration.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct CostPerChunkDurationAdjusted;
}

use phantoms::*;

impl Amount<Irys> {
    /// Used for pledge fee decay calculation
    ///
    /// The formula is: pledge_value = pledge_base_fee / ((current_pledge_count + 1) ^ decay_rate)
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn apply_pledge_decay(
        self,
        current_pledge_count: u64,
        decay_rate: Amount<Percentage>,
    ) -> Result<Self> {
        // The formula is: pledge_value = pledge_base_fee / ((count + 1) ^ decay_rate)
        // We use the identity: x^y = exp(y * ln(x))
        let count_plus_one = current_pledge_count + 1;

        // Convert count+1 to fixed-point
        let count_fp18 = safe_mul(U256::from(count_plus_one), TOKEN_SCALE)?;

        // Calculate ln(count+1) in fixed-point
        let ln_count = ln_fp18(count_fp18)?;

        // Convert decay_rate from precision scale to TOKEN_SCALE
        // decay_rate is in PRECISION_SCALE (1e12), we need it in TOKEN_SCALE (1e18)
        let decay_rate_fp18 = mul_div(decay_rate.amount, TOKEN_SCALE, PRECISION_SCALE)?;

        // Calculate decay_rate * ln(count+1)
        let exponent = mul_div(decay_rate_fp18, ln_count, TOKEN_SCALE)?;

        // Calculate exp(decay_rate * ln(count+1)) = (count+1)^decay_rate
        let divisor = exp_fp18(exponent)?;

        // Calculate pledge_base_fee / divisor
        let adjusted_amount = mul_div(self.amount, TOKEN_SCALE, divisor)?;

        Ok(Self {
            amount: adjusted_amount,
            _t: PhantomData,
        })
    }
}

/// Implements cost calculation for 1 chunk/epoch storage in USD.
impl Amount<(CostPerChunk, Usd)> {
    /// Calculate the total cost for storage.
    /// The price is for storing a single replica.
    ///
    /// n = epochs to pay for storage
    /// r = decay rate per epoch
    ///
    /// total cost = `cost_per_epoch` * ((1 - (1-r)^n) / r)
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn cost_per_replica(
        self,
        epochs: u64,
        decay_rate: Amount<DecayRate>,
    ) -> Result<Amount<(CostPerChunkDurationAdjusted, Usd)>> {
        // Calculate (1 - r) in precision scale.
        let one_minus_r = safe_sub(PRECISION_SCALE, decay_rate.amount)?;

        // Compute (1 - r)^n in precision scale using precision_pow (the correct scale).
        let pow_val = precision_pow(one_minus_r, epochs)?;

        // numerator = (1 - (1 - r)^n) in precision scale.
        let numerator = safe_sub(PRECISION_SCALE, pow_val)?;

        // fraction_ps = numerator / r => (numerator * PRECISION_SCALE / r)
        let fraction_ps = mul_div(numerator, PRECISION_SCALE, decay_rate.amount)?;

        // Convert fraction from precision scale to 1e18 fixed point:
        let fraction_1e18 = mul_div(fraction_ps, TOKEN_SCALE, PRECISION_SCALE)?;

        // Multiply the cost per epoch by the fraction.
        let total = mul_div(self.amount, fraction_1e18, TOKEN_SCALE)?;

        Ok(Amount {
            amount: total,
            _t: PhantomData,
        })
    }

    // Assuming you have a method to calculate cost for multiple replicas.
    pub fn replica_count(self, count: u64) -> Result<Self> {
        let count_u256 = U256::from(count);
        let total = safe_mul(self.amount, count_u256)?;
        Ok(Self {
            amount: total,
            _t: PhantomData,
        })
    }
}

/// For cost of storing 1 chunk per epoch in USD, already adjusted for a certain period.
impl Amount<(CostPerChunkDurationAdjusted, Usd)> {
    /// Apply a multiplier of how much would storing the data cost for `n` replicas.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn replica_count(self, replicas: u64) -> Result<Self> {
        // safe_mul for scale * integer is okay (the result is still scaled)
        let total = safe_mul(self.amount, U256::from(replicas))?;
        Ok(Self {
            amount: total,
            ..self
        })
    }

    /// Calculate the network fee, denominated in $IRYS tokens
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn base_network_fee(
        self,
        bytes_to_store: U256,
        chunk_size: u64,
        irys_token_price: Amount<(IrysPrice, Usd)>,
    ) -> Result<Amount<(NetworkFee, Irys)>> {
        // Calculate number of chunks (rounded up)
        let chunk_size_u256 = U256::from(chunk_size);
        let num_chunks = safe_div(
            safe_add(bytes_to_store, safe_sub(chunk_size_u256, U256::one())?)?,
            chunk_size_u256,
        )?;

        // usd_fee = self.amount * num_chunks
        let usd_fee = safe_mul(self.amount, num_chunks)?;

        // IRYS = usd_fee / token_price
        let network_fee = mul_div(usd_fee, TOKEN_SCALE, irys_token_price.amount)?;

        Ok(Amount {
            amount: network_fee,
            _t: PhantomData,
        })
    }
}

impl Amount<(NetworkFee, Irys)> {
    /// Add additional network fee for storing data to increase incentivization.
    /// Percentage must be expressed using PRECISION_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn add_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = amount * (1 + percentage) / SCALE
        let one_plus = safe_add(PRECISION_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_plus, PRECISION_SCALE)?;
        Ok(Self {
            amount: total,
            _t: PhantomData,
        })
    }

    /// Add ingress proof rewards to the base network fee.
    ///
    /// According to business rules:
    /// - Each ingress proof gets a reward of (fee_percentage × term_fee)
    /// - Total ingress rewards = num_ingress_proofs × (fee_percentage × term_fee)
    /// - Final perm_fee = base_network_fee + total_ingress_rewards
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn add_ingress_proof_rewards(
        self,
        term_fee: U256,
        num_ingress_proofs: u64,
        fee_percentage: Amount<Percentage>,
    ) -> Result<Self> {
        // Calculate reward per ingress proof (fee_percentage × term_fee)
        let per_ingress_reward = mul_div(term_fee, fee_percentage.amount, PRECISION_SCALE)?;

        // Calculate total ingress rewards
        let total_ingress_rewards =
            per_ingress_reward.saturating_mul(U256::from(num_ingress_proofs));

        // Add ingress rewards to base network fee
        let total = self.amount.saturating_add(total_ingress_rewards);

        Ok(Self {
            amount: total,
            _t: PhantomData,
        })
    }
}

impl Amount<(IrysPrice, Usd)> {
    /// Calculate the Exponential Moving Average for the current Irys Price (denominated in $USD).
    ///
    /// The EMA can be calculated using the following formula:
    ///
    /// `EMA b = α ⋅ Pb + (1 - α) ⋅ EMAb-1`
    ///
    /// Where:
    /// - `EMAb` is the Exponential Moving Average at block b.
    /// - `α` is the smoothing factor, calculated as `α = 2 / (n+1)`, where n is the number of block prices.
    /// - `Pb` is the price at block b.
    /// - `EMAb-1` is the EMA at the previous block.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn calculate_ema(self, total_past_blocks: u64, previous_ema: Self) -> Result<Self> {
        // denominator = n+1
        let denom = U256::from(total_past_blocks)
            .checked_add(U256::one())
            .ok_or_else(|| eyre!("failed to compute total_past_blocks + 1"))?;

        // alpha = 2e18 / (denom*1e18) => we do alpha = mul_div(2*SCALE, SCALE, denom*SCALE)
        // simpler: alpha = (2*SCALE) / (denom), then we consider dividing by SCALE afterwards
        let two_scale = safe_mul(U256::from(2_u64), TOKEN_SCALE)?;
        let alpha = safe_div(two_scale, denom)?; // alpha is scaled 1e18

        // Check alpha in (0,1e18]
        ensure!(
            alpha > U256::zero() && alpha <= TOKEN_SCALE,
            "alpha out of range"
        );

        // (1 - alpha)
        let one_minus_alpha = safe_sub(TOKEN_SCALE, alpha)?;

        // scaled_current = alpha * currentPrice / 1e18
        let scaled_current = mul_div(self.amount, alpha, TOKEN_SCALE)?;

        // scaled_last = (1 - alpha) * prevEMA / 1e18
        let scaled_last = mul_div(previous_ema.amount, one_minus_alpha, TOKEN_SCALE)?;

        // sum
        let ema_value = safe_add(scaled_current, scaled_last)?;

        Ok(Self::new(ema_value))
    }

    /// Add extra percentage on top of the existing price.
    /// Percentage must be expressed using PRECISION_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn add_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = amount * (1 + percentage) / SCALE
        let one_plus = safe_add(PRECISION_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_plus, PRECISION_SCALE)?;
        Ok(Self::new(total))
    }

    /// Remove extra percentage on top of the existing price.
    /// Percentage must be expressed using PRECISION_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn sub_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = (amount * (1 - percentage)) / PRECISION_SCALE
        let one_minus = safe_sub(PRECISION_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_minus, PRECISION_SCALE)?;
        Ok(Self::new(total))
    }
}

impl<T> core::fmt::Display for Amount<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.amount)
    }
}

impl<T> Debug for Amount<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.amount)
    }
}

/// Exponentiation by squaring for precision scale:
/// (base_ps / PRECISION_SCALE)^exp, returning a result scaled by PRECISION_SCALE.
fn precision_pow(mut base_ps: U256, mut exp: u64) -> Result<U256> {
    // Start with 1 in precision scale.
    let mut result = PRECISION_SCALE;
    while exp > 0 {
        if (exp & 1) == 1 {
            // Multiply: result = result * base_ps / PRECISION_SCALE
            result = mul_div(result, base_ps, PRECISION_SCALE)?;
        }
        base_ps = mul_div(base_ps, base_ps, PRECISION_SCALE)?;
        exp >>= 1;
    }
    Ok(result)
}

/// safe addition that errors on overflow.
#[tracing::instrument(err)]
pub fn safe_add(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_add(rhs).ok_or_else(|| eyre!("overflow in add"))
}

/// safe subtraction that errors on underflow.
#[tracing::instrument(err)]
pub fn safe_sub(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| eyre!("underflow in sub"))
}

/// safe multiplication that errors on overflow.
#[tracing::instrument(err)]
pub fn safe_mul(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_mul(rhs).ok_or_else(|| eyre!("overflow in mul"))
}

/// safe division that errors on division-by-zero.
#[tracing::instrument(err)]
pub fn safe_div(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("division by zero"))
    } else {
        Ok(lhs.checked_div(rhs).unwrap())
    }
}

#[tracing::instrument(err)]
pub fn safe_mod(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("module by zero"))
    } else {
        Ok(lhs % rhs)
    }
}

/// computes (a * b) / c in 256-bit arithmetic with checks.
#[tracing::instrument]
pub fn mul_div(mul_lhs: U256, mul_rhs: U256, div: U256) -> Result<U256> {
    let prod = safe_mul(mul_lhs, mul_rhs)?;
    safe_div(prod, div)
}

/// Computes the natural logarithm ln(x) in 18-decimal fixed-point
/// Input x must be in TOKEN_SCALE (1e18 = 1.0)
/// Uses the Taylor series: ln(1+y) = y - y²/2 + y³/3 - y⁴/4 + ...
/// For better convergence, we use: ln(x) = ln(2^k * m) = k*ln(2) + ln(m)
/// where m is in range [1, 2)
fn ln_fp18(x: U256) -> Result<U256> {
    if x.is_zero() {
        return Err(eyre!("ln(0) is undefined"));
    }

    const TWO_FP18: U256 = U256([2_000_000_000_000_000_000_u64, 0, 0, 0]);

    // Find k such that x / 2^k is in range [1, 2)
    let mut k = 0_u32;
    let mut m = x;

    // Scale down by powers of 2 until m < 2
    while m >= TWO_FP18 {
        m = safe_div(m, U256::from(2))?;
        k += 1;
    }

    // Scale up by powers of 2 until m >= 1
    while m < TOKEN_SCALE {
        m = safe_mul(m, U256::from(2))?;
        k = k.saturating_sub(1);
    }

    // Now m is in [1, 2), compute ln(m) using Taylor series
    // Convert to y = m - 1, so y is in [0, 1)
    let y = safe_sub(m, TOKEN_SCALE)?;

    // Taylor series: ln(1+y) = y - y²/2 + y³/3 - y⁴/4 + ...
    let mut sum = U256::zero();
    let mut y_power = y; // y^i

    for i in 1..=TAYLOR_TERMS {
        let term = safe_div(y_power, U256::from(i))?;

        if i % 2 == 1 {
            sum = safe_add(sum, term)?;
        } else {
            sum = safe_sub(sum, term)?;
        }

        // Update y_power for next iteration
        y_power = mul_div(y_power, y, TOKEN_SCALE)?;
    }

    // Result = k * ln(2) + ln(m)
    let k_ln2 = safe_mul(U256::from(k), LN2_FP18)?;
    let result = safe_add(k_ln2, sum)?;

    Ok(result)
}

/// Computes exp(x) in 18-decimal fixed-point using Taylor series
/// Input x must be in TOKEN_SCALE (1e18 = 1.0)
fn exp_fp18(x: U256) -> Result<U256> {
    let mut term = TOKEN_SCALE; // first term is 1
    let mut sum = TOKEN_SCALE; // accumulated sum

    for i in 1..=TAYLOR_TERMS {
        term = mul_div(term, x, TOKEN_SCALE)?; // multiply by x
        term = safe_div(term, U256::from(i))?; // divide by i
        sum = safe_add(sum, term)?;
    }

    Ok(sum)
}

/// Computes exp(-x) in 18-decimal fixed-point using Taylor series
/// Input x must be in TOKEN_SCALE (1e18 = 1.0)
/// Uses the expansion: exp(-x) = 1 - x + x²/2! - x³/3! + x⁴/4! - ...
pub fn exp_neg_fp18(x: U256) -> Result<U256> {
    let mut term = TOKEN_SCALE; // first term is 1
    let mut sum = TOKEN_SCALE; // accumulated sum

    for i in 1..=TAYLOR_TERMS {
        term = mul_div(term, x, TOKEN_SCALE)?; // multiply by x
        term = safe_div(term, U256::from(i))?; // divide by i
        sum = if i & 1 == 1 {
            // subtract on odd steps
            safe_sub(sum, term)?
        } else {
            // add on even steps
            safe_add(sum, term)?
        };
    }
    Ok(sum)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "used in tests")]
#[expect(clippy::panic_in_result_fn, reason = "used in tests")]
mod tests {
    use super::*;
    use eyre::Result;
    use rust_decimal_macros::dec;

    #[test]
    fn test_amount_rlp_round_trip() {
        // setup
        let data = Amount::<IrysPrice>::token(dec!(1.11)).unwrap();

        // action
        let mut buffer = vec![];
        data.encode(&mut buffer);
        let decoded = Amount::<IrysPrice>::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    mod token_conversions {
        use super::*;

        #[test]
        fn test_decimal_to_u256_known_values() {
            // 1 token should convert to exactly 1e18.
            let one_token = dec!(1);
            let one_token_u256 =
                Amount::<()>::decimal_to_u256(one_token, TOKEN_SCALE_NATIVE).unwrap();
            assert_eq!(one_token_u256, U256::from(TOKEN_SCALE_NATIVE));

            // 0.5 token => 0.5 * 1e18 = 5e17.
            let half_token = dec!(0.5);
            let half_token_u256 =
                Amount::<()>::decimal_to_u256(half_token, TOKEN_SCALE_NATIVE).unwrap();
            assert_eq!(half_token_u256, U256::from(TOKEN_SCALE_NATIVE / 2));

            // The minimum token unit, 0.000000000000000001, should become 1.
            let min_token = dec!(0.000000000000000001);
            let min_token_u256 =
                Amount::<()>::decimal_to_u256(min_token, TOKEN_SCALE_NATIVE).unwrap();
            assert_eq!(min_token_u256, U256::from(1));

            // 1_000_000_000 token should convert to exactly 1e27.
            let large_tokens = dec!(1_000_000_000);
            let one_token_u256 =
                Amount::<()>::decimal_to_u256(large_tokens, TOKEN_SCALE_NATIVE).unwrap();
            assert_eq!(
                one_token_u256,
                U256::from((TOKEN_SCALE_NATIVE as u128) * 1_000_000_000)
            );
        }

        #[test]
        fn test_u256_to_decimal_known_values() {
            // 1e18 as U256 should become exactly 1 token.
            let one_token_u256 = TOKEN_SCALE;
            let one_token_dec = Amount::<()>::new(one_token_u256)
                .token_to_decimal()
                .unwrap();
            assert_eq!(one_token_dec, dec!(1));

            // 5e17 as U256 should convert to 0.5.
            let half_token_u256 = TOKEN_SCALE / 2;
            let half_token_dec = Amount::<()>::new(half_token_u256)
                .token_to_decimal()
                .unwrap();
            assert_eq!(half_token_dec, dec!(0.5));

            // A U256 value of 1 should be 0.000000000000000001.
            let min_token_u256 = U256::from(1);
            let min_token_dec = Amount::<()>::new(min_token_u256)
                .token_to_decimal()
                .unwrap();
            assert_eq!(min_token_dec, dec!(0.000000000000000001));
        }
    }

    mod cost_per_chunk {
        use super::*;
        use eyre::Result;
        use rust_decimal::Decimal;
        use rust_decimal_macros::dec;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup:
            // This test verifies the mathematical formula produces consistent results
            // Golden values from original test: 0.01 cost, 200 periods, 1% decay = 0.8661 total
            
            // We use abstract values that match the original test to maintain golden values
            let cost_per_period = Amount::token(dec!(0.01)).unwrap();
            let decay_rate = Amount::percentage(dec!(0.01)).unwrap(); // 1% per period
            let periods = 200;

            // Action
            let total_cost = cost_per_period.cost_per_replica(periods, decay_rate)?.replica_count(1)?;

            // Convert the result to Decimal for comparison
            let actual = total_cost.token_to_decimal().unwrap();

            // Assert - should match the golden value
            let expected = dec!(0.8661);
            let diff = (actual - expected).abs();
            assert!(
                diff < dec!(0.0001),
                "actual={}, expected={}",
                actual,
                expected
            );

            // Check cost for 10 replicas => multiply by 10
            let cost_10 = total_cost.replica_count(10)?;
            let actual_10 = cost_10.token_to_decimal().unwrap();
            let expected_10 = dec!(8.66);
            let diff_10 = (actual_10 - expected_10).abs();
            assert!(
                diff_10 < dec!(0.001),
                "actual={}, expected={}",
                actual_10,
                expected_10
            );

            Ok(())
        }

        #[test]
        // r = 0 => division by zero => should error.
        fn test_zero_decay_rate() {
            // cost_per_epoch = 1000 (scaled 1e18)
            // decay = 0 BPS => division by zero.
            let cost_per_epoch = Amount::token(dec!(1000)).unwrap();
            let decay = Amount::percentage(dec!(0)).unwrap();
            let epochs = 10;

            let result = cost_per_epoch.cost_per_replica(epochs, decay);

            // Expect an error.
            assert!(result.is_err(), "Expected an error for r=0, got Ok(...)");
        }

        #[test]
        // r = 1 => fraction = (1 - (1 - 1)^n)/1 = 1,
        fn test_full_decay_rate() -> Result<()> {
            // cost_per_epoch = 500 (scaled 1e18)
            // decay = 100% (BPS_SCALE)
            let cost_per_epoch = Amount::token(dec!(500)).unwrap();
            let decay = Amount::percentage(dec!(1.0)).unwrap(); // 100%
            let epochs_to_pay_for_storage = 5;

            let total = cost_per_epoch
                .cost_per_replica(epochs_to_pay_for_storage, decay)?
                .replica_count(1)?;

            let actual_dec = total.token_to_decimal().unwrap();
            let expected_dec = dec!(500);
            assert_eq!(
                actual_dec, expected_dec,
                "expected 500.0, got {}",
                actual_dec
            );
            Ok(())
        }

        #[test]
        fn test_decay_rate_above_one() {
            let cost_per_epoch = Amount::token(dec!(0.01)).unwrap();
            let decay = Amount::percentage(dec!(1.5)).unwrap(); // Above 100%
            let epochs = 200;

            let result = cost_per_epoch.cost_per_replica(epochs, decay);
            assert!(
                result.is_err(),
                "Expected result.is_err() for a decay rate above 1.0"
            );
        }

        #[test]
        fn test_no_epochs_to_pay() -> Result<()> {
            // If epochs = 0 => total cost = 0.
            // cost_per_epoch = 1234.56 (scaled 1e18)
            // decay = 5%
            let cost_per_epoch = Amount::token(dec!(1234.56)).unwrap();
            let decay = Amount::percentage(dec!(0.05)).unwrap(); // 5%
            let epochs = 0;

            let total = cost_per_epoch.cost_per_replica(epochs, decay)?.replica_count(1)?;

            let actual_dec = total.token_to_decimal().unwrap();
            let expected_dec = Decimal::ZERO;
            assert_eq!(actual_dec, expected_dec, "expected 0.0, got {}", actual_dec);
            Ok(())
        }

        #[test]
        // If cost per epoch = 0 => total = 0, regardless of decay rate.
        fn test_cost_per_epoch_zero() -> Result<()> {
            // cost_per_epoch = 0
            // decay = 5%
            let cost_per_epoch = Amount::token(dec!(0)).unwrap();
            let decay = Amount::percentage(dec!(0.05)).unwrap(); // 5%
            let epochs = 10;

            let total = cost_per_epoch.cost_per_replica(epochs, decay)?.replica_count(1)?;

            let actual_dec = total.token_to_decimal().unwrap();
            assert_eq!(
                actual_dec,
                Decimal::ZERO,
                "expected 0.0, got {}",
                actual_dec
            );
            Ok(())
        }
    }

    mod user_fee {
        use super::*;
        use rust_decimal_macros::dec;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup: Testing base_network_fee calculation
            // Using simple values for clarity
            let cost_per_chunk_adjusted = Amount::token(dec!(0.001)).unwrap(); // 0.001 USD per chunk (after duration adjustment)
            let price_irys = Amount::token(dec!(1.0)).unwrap(); // 1 USD per IRYS token
            let bytes_to_store = 256_u64 * 1024_u64 * 10_u64; // 10 chunks worth
            let chunk_size = 256_u64 * 1024_u64; // 256 KiB
            let fee_percentage = Amount::percentage(dec!(0.05)).unwrap(); // 5%

            // Action
            let network_fee = cost_per_chunk_adjusted
                .base_network_fee(U256::from(bytes_to_store), chunk_size, price_irys)?;
            let price_with_network_reward = network_fee.add_multiplier(fee_percentage)?;

            // Convert results for checking
            let network_fee_dec = network_fee.token_to_decimal().unwrap();
            let reward_dec = price_with_network_reward.token_to_decimal().unwrap();

            // Expected: 10 chunks * 0.001 USD per chunk / 1.0 USD per IRYS = 0.01 IRYS
            let expected = dec!(0.01);
            let diff = (network_fee_dec - expected).abs();
            assert!(
                diff < dec!(0.0000001),
                "network_fee = {}, expected = {}",
                network_fee_dec,
                expected
            );

            // Assert with 5% multiplier: 0.01 * 1.05 = 0.0105
            let expected2 = dec!(0.0105);
            let diff2 = (reward_dec - expected2).abs();
            assert!(
                diff2 < dec!(0.0000001),
                "with multiplier = {}, expected = {}",
                reward_dec,
                expected2
            );
            Ok(())
        }
    }

    mod ema_calculations {
        use super::*;
        use rust_decimal_macros::dec;

        #[test]
        fn test_calculate_ema_valid() -> Result<()> {
            // Setup
            let total_past_blocks = 10;
            let ema_0 = Amount::token(dec!(1.00)).unwrap();
            let current_price = Amount::token(dec!(1.01)).unwrap();

            // Action
            let ema_1 = current_price.calculate_ema(total_past_blocks, ema_0)?;

            // Compare
            let actual = ema_1.token_to_decimal().unwrap();
            let expected = dec!(1.00181818181818);
            let diff = (actual - expected).abs();
            assert!(
                diff < dec!(0.00000001),
                "EMA is {}, expected around {}",
                actual,
                expected
            );
            Ok(())
        }

        #[test]
        fn test_calculate_ema_huge_epoch() {
            let total_past_blocks = u64::MAX;
            let current_irys_price = Amount::token(dec!(123.456)).unwrap();
            let last_block_ema = Amount::token(dec!(1000.0)).unwrap();

            let result = current_irys_price.calculate_ema(total_past_blocks, last_block_ema);
            assert!(result.is_err());
        }
    }

    mod multipliers {
        use super::*;
        use eyre::Result;
        use rust_decimal_macros::dec;

        #[test]
        fn test_add_multiplier_normal_case() -> Result<()> {
            // Original amount = 100.0 usd
            let original = Amount::<(IrysPrice, Usd)>::token(dec!(100.0))?;
            // +10%
            let ten_percent = Amount::<Percentage>::percentage(dec!(0.10))?;

            // Action
            let result = original.add_multiplier(ten_percent)?;

            // Convert to decimal for comparison
            let actual = result.token_to_decimal()?;
            let expected = dec!(110.0); // 100 + 10%

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_add_multiplier_zero_percent() -> Result<()> {
            // Original amount = 42 usd
            let original = Amount::<(IrysPrice, Usd)>::token(dec!(42.0))?;
            let zero_percent = Amount::<Percentage>::percentage(dec!(0.0))?;

            let result = original.add_multiplier(zero_percent)?;

            let actual = result.token_to_decimal()?;
            let expected = dec!(42.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_sub_multiplier_normal_case() -> Result<()> {
            // Original amount = 100.0 usd
            let original = Amount::<(IrysPrice, Usd)>::token(dec!(100.0))?;
            // -5%
            let five_percent = Amount::<Percentage>::percentage(dec!(0.05))?;

            let result = original.sub_multiplier(five_percent)?;
            let actual = result.token_to_decimal()?;
            let expected = dec!(95.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_sub_multiplier_results_in_zero() -> Result<()> {
            // If original is 0, sub X% remains 0
            let original = Amount::<(IrysPrice, Usd)>::token(dec!(0.0))?;
            let any_percent = Amount::<Percentage>::percentage(dec!(0.30))?; // 30%

            let result = original.sub_multiplier(any_percent)?;
            let actual = result.token_to_decimal()?;
            let expected = dec!(0.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_sub_multiplier_above_100_percent_fails() {
            // If the percentage is above 1.0 (100%), sub_multiplier should error out.
            let original = Amount::<(IrysPrice, Usd)>::token(dec!(50.0)).unwrap();
            let above_one = Amount::<Percentage>::percentage(dec!(1.10)).unwrap(); // 110%

            let result = original.sub_multiplier(above_one);
            assert!(result.is_err(), "Expected error for sub > 100%");
        }

        #[test]
        fn test_add_ingress_proof_rewards_normal_case() -> Result<()> {
            // Base network fee = 100 IRYS
            let base_fee = Amount::<(NetworkFee, Irys)>::token(dec!(100.0))?;
            // Term fee = 10 IRYS
            let term_fee = Amount::<Irys>::token(dec!(10.0))?.amount;
            // 5% miner fee
            let five_percent = Amount::<Percentage>::percentage(dec!(0.05))?;
            // 3 ingress proofs
            let num_proofs = 3;

            // Action
            let result = base_fee.add_ingress_proof_rewards(term_fee, num_proofs, five_percent)?;

            // Convert to decimal for comparison
            let actual = result.token_to_decimal()?;
            // Expected: 100 + (3 * 10 * 0.05) = 100 + 1.5 = 101.5
            let expected = dec!(101.5);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_add_ingress_proof_rewards_zero_proofs() -> Result<()> {
            // Base network fee = 50 IRYS
            let base_fee = Amount::<(NetworkFee, Irys)>::token(dec!(50.0))?;
            let term_fee = Amount::<Irys>::token(dec!(10.0))?.amount;
            let five_percent = Amount::<Percentage>::percentage(dec!(0.05))?;
            let num_proofs = 0;

            // Action
            let result = base_fee.add_ingress_proof_rewards(term_fee, num_proofs, five_percent)?;

            // Convert to decimal for comparison
            let actual = result.token_to_decimal()?;
            // Expected: 50 + (0 * 10 * 0.05) = 50
            let expected = dec!(50.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_add_ingress_proof_rewards_zero_percentage() -> Result<()> {
            // Base network fee = 75 IRYS
            let base_fee = Amount::<(NetworkFee, Irys)>::token(dec!(75.0))?;
            let term_fee = Amount::<Irys>::token(dec!(20.0))?.amount;
            let zero_percent = Amount::<Percentage>::percentage(dec!(0.0))?;
            let num_proofs = 5;

            // Action
            let result = base_fee.add_ingress_proof_rewards(term_fee, num_proofs, zero_percent)?;

            // Convert to decimal for comparison
            let actual = result.token_to_decimal()?;
            // Expected: 75 + (5 * 20 * 0.0) = 75
            let expected = dec!(75.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }

        #[test]
        fn test_add_ingress_proof_rewards_large_values() -> Result<()> {
            // Base network fee = 1000 IRYS
            let base_fee = Amount::<(NetworkFee, Irys)>::token(dec!(1000.0))?;
            // Term fee = 100 IRYS
            let term_fee = Amount::<Irys>::token(dec!(100.0))?.amount;
            // 10% fee
            let ten_percent = Amount::<Percentage>::percentage(dec!(0.10))?;
            // 10 ingress proofs
            let num_proofs = 10;

            // Action
            let result = base_fee.add_ingress_proof_rewards(term_fee, num_proofs, ten_percent)?;

            // Convert to decimal for comparison
            let actual = result.token_to_decimal()?;
            // Expected: 1000 + (10 * 100 * 0.10) = 1000 + 100 = 1100
            let expected = dec!(1100.0);

            assert_eq!(expected, actual, "Expected {}, got {}", expected, actual);
            Ok(())
        }
    }

    mod exp_neg {
        use super::*;
        use rust_decimal_macros::dec;

        #[test]
        fn test_exp_neg_fp18_zero() -> Result<()> {
            // exp(-0) = 1
            let result = exp_neg_fp18(U256::zero())?;
            assert_eq!(result, TOKEN_SCALE);
            Ok(())
        }

        #[test]
        fn test_exp_neg_fp18_small_value() -> Result<()> {
            // Test exp(-0.1) ≈ 0.9048374180359595
            let x = Amount::<()>::token(dec!(0.1))?.amount;
            let result = exp_neg_fp18(x)?;
            let result_dec = Amount::<()>::new(result).token_to_decimal()?;

            let expected = dec!(0.9048374180359595);
            let diff = (result_dec - expected).abs();
            assert!(
                diff <= dec!(0.000000000000001),
                "exp(-0.1) = {}, expected {}",
                result_dec,
                expected
            );
            Ok(())
        }

        #[test]
        fn test_exp_neg_fp18_one() -> Result<()> {
            // Test exp(-1) ≈ 0.36787944117144233
            let x = TOKEN_SCALE;
            let result = exp_neg_fp18(x)?;
            let result_dec = Amount::<()>::new(result).token_to_decimal()?;

            let expected = dec!(0.36787944117144233);
            let diff = (result_dec - expected).abs();
            assert!(
                diff <= dec!(0.000000000000001),
                "exp(-1) = {}, expected {}",
                result_dec,
                expected
            );
            Ok(())
        }

        #[test]
        fn test_exp_neg_fp18_consistency_with_exp() -> Result<()> {
            // Test that exp(-x) * exp(x) ≈ 1
            let x = Amount::<()>::token(dec!(0.5))?.amount;

            let exp_neg_x = exp_neg_fp18(x)?;
            let exp_x = exp_fp18(x)?;

            // exp(-x) * exp(x) should equal 1
            let product = mul_div(exp_neg_x, exp_x, TOKEN_SCALE)?;
            let product_dec = Amount::<()>::new(product).token_to_decimal()?;

            let diff = (product_dec - dec!(1.0)).abs();
            assert!(
                diff <= dec!(0.000000000000001),
                "exp(-x) * exp(x) = {}, expected 1.0",
                product_dec
            );
            Ok(())
        }
    }
}
