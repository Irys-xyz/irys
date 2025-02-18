//! A utility module for calculating netwokrk fees, costs for storing different amounts of data, and EMA for blocks.
//!
//! Core data types:
//! - `Amount<(CostPerGb, Usd)>` - Cost in $USD of storing 1GB on irys (per single replica), data part of the config
//! - `Amount<(IrysPrice, Usd)>` - Cost in $USD of a single $IRYS token, the data retrieved form oracles
//! - `Amount<(Ema, Usd)>` - Exponential Moving Average for a single $IRYS token, the data to be stored in blocks
//! - `Amount<(NetworkFee, Irys)>` - The cost in $IRYS that the user will have to pay to store his data on Irys

use crate::U256;
use alloy_rlp::{Decodable, Encodable};
use arbitrary::Arbitrary;
use core::{fmt::Debug, marker::PhantomData, ops::Deref};
use eyre::{ensure, eyre, Result};
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

/// 1.0 in 18-decimal fixed point. little endian encoded.
/// Used by token price representations.
pub const PRICE_SCALE: U256 = U256([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// Basis points scale representation.
/// 100% - 1_000_000 as little endian number.
/// Used by percentage representations.
pub const BPS_SCALE: U256 = U256([1_000_000, 0, 0, 0]);

/// `Amount<T>` represents a value stored as a U256.
///
/// The actual scale is defined by the usage: pr
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary, Default,
)]
pub struct Amount<T> {
    amount: U256,
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

impl<T> Decodable for Amount<T> {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let res = U256::decode(buf)?;
        Ok(Amount::new(res))
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
            Amount {
                amount: instance,
                _t: PhantomData,
            },
            buf,
        )
    }
}

impl<T> Amount<T> {
    #[must_use]
    pub const fn new(amount: U256) -> Self {
        Self {
            amount,
            _t: PhantomData,
        }
    }
}

impl<T> Deref for Amount<T> {
    type Target = U256;
    fn deref(&self) -> &Self::Target {
        &self.amount
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

    /// Exponential Moving Average.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct Ema;

    /// Decay rate to account for storage hardware getting cheaper.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary)]
    pub struct DecayRate;

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
        // fraction_1e18 = fraction_bps * 1e18 / 10000
        let fraction_1e18 = mul_div(fraction_bps, PRICE_SCALE, BPS_SCALE)?;

        // Multiply the annual cost by the fraction.
        let total = mul_div(self.amount, fraction_1e18, PRICE_SCALE)?;

        Ok(Amount {
            amount: total,
            _t: PhantomData,
        })
    }

    // Assuming you have a method to calculate cost for multiple replicas.
    pub fn replica_count(self, count: u64) -> Result<Self> {
        let count_u256 = U256::from(count);
        let total = safe_mul(self.amount, count_u256)?;
        Ok(Amount {
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
    pub fn base_network_fee(
        self,
        bytes_to_store: U256,
        irys_token_price: Amount<(IrysPrice, Usd)>,
    ) -> Result<Amount<(NetworkFee, Irys)>> {
        // We treat bytes_to_store as a pure integer.
        let bytes_in_gb = U256::from(1_073_741_824u64); // 1024 * 1024 * 1024
        let ratio = mul_div(bytes_to_store, PRICE_SCALE, bytes_in_gb)?;

        // usd_fee = self.amount * ratio / SCALE
        let usd_fee = mul_div(self.amount, ratio, PRICE_SCALE)?;

        // IRYS = usd_fee / token_price
        let network_fee = mul_div(usd_fee, PRICE_SCALE, irys_token_price.amount)?;

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
    pub fn add_multiplier(self, percentage: U256) -> Result<Self> {
        // total = amount * (1 + percentage) / SCALE
        let one_plus = safe_add(BPS_SCALE, percentage)?;
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
    pub fn calculate_ema(
        self,
        total_past_blocks: u64,
        previous_ema: Amount<(Ema, Usd)>,
    ) -> Result<Amount<(Ema, Usd)>> {
        // denominator = n+1
        let denom = U256::from(total_past_blocks)
            .checked_add(U256::one())
            .ok_or_else(|| eyre!("failed to compute total_past_blocks + 1"))?;

        // alpha = 2e18 / (denom*1e18) => we do alpha = mul_div(2*SCALE, SCALE, denom*SCALE)
        // simpler: alpha = (2*SCALE) / (denom), then we consider dividing by SCALE afterwards
        let two_scale = safe_mul(U256::from(2u64), PRICE_SCALE)?;
        let alpha = safe_div(two_scale, denom)?; // alpha is scaled 1e18

        // Check alpha in (0,1e18]
        ensure!(
            alpha > U256::zero() && alpha <= PRICE_SCALE,
            "alpha out of range"
        );

        // (1 - alpha)
        let one_minus_alpha = safe_sub(PRICE_SCALE, alpha)?;

        // scaled_current = alpha * currentPrice / 1e18
        let scaled_current = mul_div(self.amount, alpha, PRICE_SCALE)?;

        // scaled_last = (1 - alpha) * prevEMA / 1e18
        let scaled_last = mul_div(previous_ema.amount, one_minus_alpha, PRICE_SCALE)?;

        // sum
        let ema_value = safe_add(scaled_current, scaled_last)?;

        Ok(Amount::new(ema_value))
    }
}

/// Basic Display impl
impl<T> core::fmt::Display for Amount<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Convert to string as integer. For printing real "decimal", you'd do
        // dividing by 1e18 yourself.
        write!(f, "<{:?}>: {}", self._t, self.amount)
    }
}

/// Example exponentiation by squaring for basis points:
/// (base_bps / 10000)^exp, returning a result scaled by 10000.
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
fn safe_add(a: U256, b: U256) -> Result<U256> {
    a.checked_add(b).ok_or_else(|| eyre!("overflow in add"))
}

/// safe subtraction that errors on underflow.
fn safe_sub(a: U256, b: U256) -> Result<U256> {
    a.checked_sub(b).ok_or_else(|| eyre!("underflow in sub"))
}

/// safe multiplication that errors on overflow.
fn safe_mul(a: U256, b: U256) -> Result<U256> {
    a.checked_mul(b).ok_or_else(|| eyre!("overflow in mul"))
}

/// safe division that errors on division-by-zero.
fn safe_div(a: U256, b: U256) -> Result<U256> {
    if b.is_zero() {
        Err(eyre!("division by zero"))
    } else {
        Ok(a.checked_div(b).unwrap())
    }
}

/// computes (a * b) / c in 256-bit arithmetic with checks.
fn mul_div(a: U256, b: U256, c: U256) -> Result<U256> {
    let prod = safe_mul(a, b)?;
    safe_div(prod, c)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "used in tests")]
#[expect(clippy::panic_in_result_fn, reason = "used in tests")]
mod tests {
    use super::*;
    use eyre::Result;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// Helper to convert a U256 (with 18 decimals) into a `Decimal` for assertions.
    /// Assumes that the U256 value is small enough to fit into a u128.
    fn u256_to_decimal(value: U256) -> Decimal {
        // Compute the integer and fractional parts.
        let quotient = value / PRICE_SCALE;
        let remainder = value % PRICE_SCALE;

        // Convert quotient and remainder to u128.
        let q: u128 = u128::try_from(quotient).expect("quotient fits in u128");
        let r: u128 = u128::try_from(remainder).expect("remainder fits in u128");

        // Build the Decimal value:
        // The quotient represents the integer part,
        // while the remainder scaled by 1e-18 is the fractional part.
        Decimal::from(q) + (Decimal::from(r) / dec!(1000000000000000000))
    }

    /// Helper to convert a Decimal into a scaled U256 with 18 decimals.
    fn decimal_to_u256(dec: Decimal) -> U256 {
        // Multiply by 1e18 (as a Decimal) and round to the nearest integer.
        let scaled = (dec * dec!(1000000000000000000)).round();
        U256::from_dec_str(&scaled.to_string()).unwrap()
    }

    mod cost_per_byte {
        use super::*;
        use eyre::Result;
        use rust_decimal::Decimal;
        use rust_decimal_macros::dec;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup:
            // annual = 0.01 (scaled 1e18)
            // decay = 1%
            let annual = Amount::new(decimal_to_u256(dec!(0.01)));
            let decay = Amount::new(U256::from(10_000u64)); // 1%
            let years = 200;

            // Action
            let cost_per_gb = annual.cost_per_replica(years, decay)?.replica_count(1)?;

            // Convert the result to Decimal for comparison
            let actual = u256_to_decimal(cost_per_gb.amount);

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
            let actual_10 = u256_to_decimal(cost_10.amount);
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
            let annual = Amount::new(decimal_to_u256(dec!(1000)));
            let decay = Amount::new(U256::zero());
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
            let annual = Amount::new(decimal_to_u256(dec!(500)));
            let decay = Amount::new(BPS_SCALE); // 100%
            let years_to_pay_for_storage = 5;

            let total = annual
                .cost_per_replica(years_to_pay_for_storage, decay)?
                .replica_count(1)?;

            let actual_dec = u256_to_decimal(total.amount);
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
            let annual = Amount::new(decimal_to_u256(dec!(0.01)));
            let decay = Amount::new(U256::from(1_500_000u64)); // Above 100%
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
            let annual = Amount::new(decimal_to_u256(dec!(1234.56)));
            let decay = Amount::new(U256::from(50_000u64)); // 5%
            let years = 0;

            let total = annual.cost_per_replica(years, decay)?.replica_count(1)?;

            let actual_dec = u256_to_decimal(total.amount);
            let expected_dec = Decimal::ZERO;
            assert_eq!(actual_dec, expected_dec, "expected 0.0, got {}", actual_dec);
            Ok(())
        }

        #[test]
        // If annual cost = 0 => total = 0, regardless of decay rate.
        fn test_annual_cost_zero() -> Result<()> {
            // annual = 0
            // decay = 5%
            let annual = Amount::new(decimal_to_u256(dec!(0)));
            let decay = Amount::new(U256::from(500u64)); // 5%
            let years = 10;

            let total = annual.cost_per_replica(years, decay)?.replica_count(1)?;

            let actual_dec = u256_to_decimal(total.amount);
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
            let cost_per_gb_10_replicas_200_years = decimal_to_u256(dec!(8.65));
            let price_irys = Amount::new(decimal_to_u256(dec!(1.09)));
            let bytes_to_store = 1024u64 * 1024u64 * 200u64; // 200 MB
            let fee_percentage = U256::from(50_000u64); // 5%

            // Action
            let network_fee = Amount {
                amount: cost_per_gb_10_replicas_200_years,
                _t: PhantomData,
            }
            .base_network_fee(U256::from(bytes_to_store), price_irys)?;

            let price_with_network_reward = network_fee.add_multiplier(fee_percentage)?;

            // Convert results for checking
            let network_fee_dec = u256_to_decimal(network_fee.amount);
            let reward_dec = u256_to_decimal(price_with_network_reward.amount);

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
            let ema_0 = Amount::new(decimal_to_u256(dec!(1.00)));
            let current_price = Amount::new(decimal_to_u256(dec!(1.01)));

            // Action
            let ema_1 = current_price.calculate_ema(total_past_blocks, ema_0)?;

            // Compare
            let actual = u256_to_decimal(ema_1.amount);
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
            let current_irys_price = Amount::new(decimal_to_u256(dec!(123.456)));
            let last_block_ema = Amount::new(decimal_to_u256(dec!(1000.0)));

            let result = current_irys_price.calculate_ema(total_past_blocks, last_block_ema);
            assert!(result.is_err());
        }
    }
}
