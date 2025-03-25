//! A utility module for calculating network fees, costs for storing different amounts of data, and EMA for blocks.
//! A utility module for calculating network fees, costs for storing different amounts of data, and EMA for blocks.
//!
//! Core data types:
//! - `Amount<(CostPerGb, Usd)>` - Cost in $USD of storing 1GB on irys (per single replica), data part of the config
//! - `Amount<(IrysPrice, Usd)>` - Cost in $USD of a single $IRYS token, the data retrieved form oracles
//! - `Amount<(NetworkFee, Irys)>` - The cost in $IRYS that the user will have to pay to store his data on Irys

use crate::U256;
use alloy_rlp::{Decodable, Encodable};
use arbitrary::Arbitrary;
use core::{fmt::Debug, marker::PhantomData};
use eyre::{ensure, eyre, Result};
use reth_codecs::Compact;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// 1.0 in 18-decimal fixed point. little endian encoded.
/// Used by token price representations.
pub const TOKEN_SCALE: U256 = U256([TOKEN_SCALE_NATIVE, 0, 0, 0]);
const TOKEN_SCALE_NATIVE: u64 = 1_000_000_000_000_000_000u64;

/// Basis points scale representation.
/// 100% - `1_000_000` as little endian number.
/// Used by percentage representations.
pub const BPS_SCALE: U256 = U256([BPS_SCALE_NATIVE, 0, 0, 0]);
const BPS_SCALE_NATIVE: u64 = 1_000_000;

/// `Amount<T>` represents a value stored as a U256.
///
/// The actual scale is defined by the usage: pr
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary, Default,
)]
pub struct Amount<T> {
    pub amount: U256,
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
        let amount = Self::decimal_to_u256(amount, BPS_SCALE_NATIVE)?;
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
            10u128
                .checked_pow(dec_scale)
                .ok_or_else(|| eyre::eyre!("decimal scale too large 10^{dec_scale}"))?,
        );

        // For rounding, add half the divisor.
        let half_divisor = safe_div(divisor, U256::from(2u8))?;

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
        // Compute the integer and fractional parts.
        let quotient = safe_div(self.amount, TOKEN_SCALE)?;
        let remainder = safe_mod(self.amount, TOKEN_SCALE)?;

        // Convert quotient and remainder to u128.
        let quotient: u128 = u128::try_from(quotient).expect("quotient fits in u128");
        let remainder: u128 = u128::try_from(remainder).expect("remainder fits in u128");

        // Build the Decimal value:
        // The quotient represents the integer part,
        // while the remainder scaled by 1e-18 is the fractional part.
        let remainder = Decimal::from(remainder)
            .checked_div(dec!(1000000000000000000))
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

    /// The cost of storing a single GB of data.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct CostPerGb;

    /// Currency denomintator util type.
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

    /// Cost per storing 1GB, of data. Includes adjustment for storage duration.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct CostPerGbYearAdjusted;
}

use phantoms::*;

/// Implements cost calculation for 1GB/year storage in USD.
impl Amount<(CostPerGb, Usd)> {
    /// Calculate the total cost for storage.
    /// The price is for storing a single replica.
    ///
    /// n = years to pay for storage
    /// r = decay rate
    ///
    /// total cost = `annual_cost` * ((1 - (1-r)^n) / r)
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn cost_per_replica(
        self,
        years: u64,
        decay_rate: Amount<DecayRate>,
    ) -> Result<Amount<(CostPerGbYearAdjusted, Usd)>> {
        // Calculate (1 - r) in basis points.
        let one_minus_r = safe_sub(BPS_SCALE, decay_rate.amount)?;

        // Compute (1 - r)^n in basis points using basis_pow (the correct scale).
        let pow_val = basis_pow(one_minus_r, years)?;

        // numerator = (1 - (1 - r)^n) in basis points.
        let numerator = safe_sub(BPS_SCALE, pow_val)?;

        // fraction_bps = numerator / r => (numerator * BPS_SCALE / r)
        let fraction_bps = mul_div(numerator, BPS_SCALE, decay_rate.amount)?;

        // Convert fraction from basis points to 1e18 fixed point:
        let fraction_1e18 = mul_div(fraction_bps, TOKEN_SCALE, BPS_SCALE)?;

        // Multiply the annual cost by the fraction.
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

/// For cost of storing 1GB/year in USD, already adjusted for a certain period.
impl Amount<(CostPerGbYearAdjusted, Usd)> {
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
        irys_token_price: Amount<(IrysPrice, Usd)>,
    ) -> Result<Amount<(NetworkFee, Irys)>> {
        // We treat bytes_to_store as a pure integer.
        let bytes_in_gb = U256::from(1_073_741_824u64); // 1024 * 1024 * 1024
        let ratio = mul_div(bytes_to_store, TOKEN_SCALE, bytes_in_gb)?;

        // usd_fee = self.amount * ratio / SCALE
        let usd_fee = mul_div(self.amount, ratio, TOKEN_SCALE)?;

        // IRYS = usd_fee / token_price
        let network_fee = mul_div(usd_fee, TOKEN_SCALE, irys_token_price.amount)?;

        Ok(Amount {
            amount: network_fee,
            _t: PhantomData,
        })
    }
}

impl Amount<(NetworkFee, Irys)> {
    /// Add additional network fee for storing data to increase incentivisation.
    /// Percentage must be expressed using BPS_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn add_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = amount * (1 + percentage) / SCALE
        let one_plus = safe_add(BPS_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_plus, BPS_SCALE)?;
        Ok(Self {
            amount: total,
            _t: PhantomData,
        })
    }
}

impl Amount<(IrysPrice, Usd)> {
    /// Calculate the Exponential Moving Average for the current Irys Price (denominaed in $USD).
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
    pub fn calculate_ema(
        self,
        total_past_blocks: u64,
        previous_ema: Self,
    ) -> Result<Self> {
        // denominator = n+1
        let denom = U256::from(total_past_blocks)
            .checked_add(U256::one())
            .ok_or_else(|| eyre!("failed to compute total_past_blocks + 1"))?;

        // alpha = 2e18 / (denom*1e18) => we do alpha = mul_div(2*SCALE, SCALE, denom*SCALE)
        // simpler: alpha = (2*SCALE) / (denom), then we consider dividing by SCALE afterwards
        let two_scale = safe_mul(U256::from(2u64), TOKEN_SCALE)?;
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
    /// Percentage must be expressed using BPS_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn add_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = amount * (1 + percentage) / SCALE
        let one_plus = safe_add(BPS_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_plus, BPS_SCALE)?;
        Ok(Self::new(total))
    }

    /// Remove extra percentage on top of the existing price.
    /// Percentage must be expressed using BPS_SCALE.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    #[tracing::instrument(err)]
    pub fn sub_multiplier(self, percentage: Amount<Percentage>) -> Result<Self> {
        // total = (amount * (1 - percentage)) / BPS_SCALE
        let one_minus = safe_sub(BPS_SCALE, percentage.amount)?;
        let total = mul_div(self.amount, one_minus, BPS_SCALE)?;
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

/// Example exponentiation by squaring for basis points:
/// (`base_bps` / 10000)^exp, returning a result scaled by 10000.
fn basis_pow(mut base_bps: U256, mut exp: u64) -> Result<U256> {
    // Start with 1 in basis point scale.
    let mut result = BPS_SCALE;
    while exp > 0 {
        if (exp & 1) == 1 {
            // Multiply: result = result * base_bps / BPS_SCALE
            result = mul_div(result, base_bps, BPS_SCALE)?;
        }
        base_bps = mul_div(base_bps, base_bps, BPS_SCALE)?;
        exp >>= 1;
    }
    Ok(result)
}

/// safe addition that errors on overflow.
#[tracing::instrument(err)]
fn safe_add(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_add(rhs).ok_or_else(|| eyre!("overflow in add"))
}

/// safe subtraction that errors on underflow.
#[tracing::instrument(err)]
fn safe_sub(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| eyre!("underflow in sub"))
}

/// safe multiplication that errors on overflow.
#[tracing::instrument(err)]
fn safe_mul(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_mul(rhs).ok_or_else(|| eyre!("overflow in mul"))
}

/// safe division that errors on division-by-zero.
#[tracing::instrument(err)]
fn safe_div(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("division by zero"))
    } else {
        Ok(lhs.checked_div(rhs).unwrap())
    }
}

#[tracing::instrument(err)]
fn safe_mod(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("module by zero"))
    } else {
        Ok(lhs % rhs)
    }
}

/// computes (a * b) / c in 256-bit arithmetic with checks.
#[tracing::instrument]
fn mul_div(mul_lhs: U256, mul_rhs: U256, div: U256) -> Result<U256> {
    let prod = safe_mul(mul_lhs, mul_rhs)?;
    safe_div(prod, div)
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

    mod token_conversoins {
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

    mod cost_per_byte {
        use super::*;
        use eyre::Result;
        use rust_decimal::Decimal;
        use rust_decimal_macros::dec;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup:
            // annual = 0.01
            // decay = 1%
            let annual = Amount::token(dec!(0.01)).unwrap();
            let decay = Amount::percentage(dec!(0.01)).unwrap(); // 1%
            let years = 200;

            // Action
            let cost_per_gb = annual.cost_per_replica(years, decay)?.replica_count(1)?;

            // Convert the result to Decimal for comparison
            let actual = cost_per_gb.token_to_decimal().unwrap();

            // Assert - cost per GB (single replica) should be ~0.8661
            let expected = dec!(0.8661);
            let diff = (actual - expected).abs();
            assert!(
                diff < dec!(0.0001),
                "actual={}, expected={}",
                actual,
                expected
            );

            // Check cost for 10 replicas => multiply by 10
            let cost_10 = cost_per_gb.replica_count(10)?;
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
            // annual = 1000 (scaled 1e18)
            // decay = 0 BPS => division by zero.
            let annual = Amount::token(dec!(1000)).unwrap();
            let decay = Amount::percentage(dec!(0)).unwrap();
            let years = 10;

            let result = annual.cost_per_replica(years, decay);

            // Expect an error.
            assert!(result.is_err(), "Expected an error for r=0, got Ok(...)");
        }

        #[test]
        // r = 1 => fraction = (1 - (1 - 1)^n)/1 = 1,
        fn test_full_decay_rate() -> Result<()> {
            // annual = 500 (scaled 1e18)
            // decay = 100% (BPS_SCALE)
            let annual = Amount::token(dec!(500)).unwrap();
            let decay = Amount::percentage(dec!(1.0)).unwrap(); // 100%
            let years_to_pay_for_storage = 5;

            let total = annual
                .cost_per_replica(years_to_pay_for_storage, decay)?
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
            let annual = Amount::token(dec!(0.01)).unwrap();
            let decay = Amount::percentage(dec!(1.5)).unwrap(); // Above 100%
            let years = 200;

            let result = annual.cost_per_replica(years, decay);
            assert!(
                result.is_err(),
                "Expected result.is_err() for a decay rate above 1.0"
            );
        }

        #[test]
        fn test_no_years_to_pay() -> Result<()> {
            // If years = 0 => total cost = 0.
            // annual = 1234.56 (scaled 1e18)
            // decay = 5%
            let annual = Amount::token(dec!(1234.56)).unwrap();
            let decay = Amount::percentage(dec!(0.05)).unwrap(); // 5%
            let years = 0;

            let total = annual.cost_per_replica(years, decay)?.replica_count(1)?;

            let actual_dec = total.token_to_decimal().unwrap();
            let expected_dec = Decimal::ZERO;
            assert_eq!(actual_dec, expected_dec, "expected 0.0, got {}", actual_dec);
            Ok(())
        }

        #[test]
        // If annual cost = 0 => total = 0, regardless of decay rate.
        fn test_annual_cost_zero() -> Result<()> {
            // annual = 0
            // decay = 5%
            let annual = Amount::token(dec!(0)).unwrap();
            let decay = Amount::percentage(dec!(0.05)).unwrap(); // 5%
            let years = 10;

            let total = annual.cost_per_replica(years, decay)?.replica_count(1)?;

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
            // Setup:
            let cost_per_gb_10_replicas_200_years = Amount::token(dec!(8.65)).unwrap();
            let price_irys = Amount::token(dec!(1.09)).unwrap();
            let bytes_to_store = 1024u64 * 1024u64 * 200u64; // 200 MB
            let fee_percentage = Amount::percentage(dec!(0.05)).unwrap(); // 5%

            // Action
            let network_fee = cost_per_gb_10_replicas_200_years
                .base_network_fee(U256::from(bytes_to_store), price_irys)?;
            let price_with_network_reward = network_fee.add_multiplier(fee_percentage)?;

            // Convert results for checking
            let network_fee_dec = network_fee.token_to_decimal().unwrap();
            let reward_dec = price_with_network_reward.token_to_decimal().unwrap();

            // Assert ~1.55
            let expected = dec!(1.55);
            let diff = (network_fee_dec - expected).abs();
            assert!(diff < dec!(0.0001));

            // Assert with 5% multiplier => ~1.63
            let expected2 = dec!(1.63);
            let diff2 = (reward_dec - expected2).abs();
            assert!(diff2 < dec!(0.01));
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
    }
}
