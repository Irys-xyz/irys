//! A utility module for calculating network fees, costs for storing different amounts of data, and EMA for blocks.
//!
//! Core data types:
//! - `Amount<(CostPerChunk, Usd)>` - Cost in $USD of storing 1 chunk on irys (per single replica), data part of the config
//! - `Amount<(IrysPrice, Usd)>` - Cost in $USD of a single $IRYS token, the data retrieved form oracles
//! - `Amount<(NetworkFee, Irys)>` - The cost in $IRYS that the user will have to pay to store his data on Irys

pub use crate::U256;
use crate::{ConsensusConfig, UnixTimestamp};
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

/// `Amount<T>` represents a value stored as a U256.
///
/// The actual scale is defined by the usage: pr
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary, Default, Hash,
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

    pub fn token(amount: Decimal) -> Result<Self> {
        let amount = Self::decimal_to_u256(amount, TOKEN_SCALE_NATIVE)?;
        Ok(Self::new(amount))
    }

    pub fn percentage(amount: Decimal) -> Result<Self> {
        let amount = Self::decimal_to_u256(amount, PRECISION_SCALE_NATIVE)?;
        Ok(Self::new(amount))
    }

    /// Helper to convert a Decimal into a scaled U256.
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
    pub fn token_to_decimal(&self) -> Result<Decimal> {
        self.amount_to_decimal(TOKEN_SCALE, TOKEN_SCALE_NATIVE)
    }

    /// Helper to convert a U256 (with 6 decimals) into a `Decimal` for assertions.
    /// Assumes that the U256 value is small enough to fit into a u128.
    pub fn percentage_to_decimal(&self) -> Result<Decimal> {
        self.amount_to_decimal(PRECISION_SCALE, PRECISION_SCALE_NATIVE)
    }

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
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct CostPerChunk;

    /// The cost of storing 1 GB of data.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct CostPerGb;

    /// Currency denominator util type.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct Usd;

    /// Currency denominator util type.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct Irys;

    /// Decay rate to account for storage hardware getting cheaper.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct DecayRate;

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct Percentage;

    /// The network fee, that the user would have to pay for storing his data on Irys.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct NetworkFee;

    /// Price of the $IRYS token.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
    pub struct IrysPrice;

    /// Cost per storing 1 chunk of data. Includes adjustment for storage duration.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Hash)]
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
    pub fn cost_per_replica(
        self,
        epochs: u64,
        decay_rate: Amount<DecayRate>,
    ) -> Result<Amount<(CostPerChunkDurationAdjusted, Usd)>> {
        // Special case: if decay_rate is 0, the cost is simply epochs * cost_per_epoch
        if decay_rate.amount.is_zero() {
            let total = safe_mul(self.amount, U256::from(epochs))?;
            return Ok(Amount {
                amount: total,
                _t: PhantomData,
            });
        }

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

/// Calculate term fee for temporary storage with dynamic epoch count
/// Uses the same replica count as permanent storage but for the specified number of epochs
pub fn calculate_term_fee(
    bytes_to_store: u64,
    epochs_for_storage: u64,
    config: &ConsensusConfig,
    number_of_ingress_proofs_total: u64,
    ema_price: Amount<(IrysPrice, Usd)>,
    timestamp: UnixTimestamp,
) -> Result<U256> {
    let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch_at(timestamp)?;

    // Apply duration (no decay for short-term storage)
    let zero_decay = Amount::percentage(Decimal::ZERO)?;
    let cost_per_chunk_duration =
        cost_per_chunk_per_epoch.cost_per_replica(epochs_for_storage, zero_decay)?;

    // Apply same replica count as perm storage
    let cost_with_replicas =
        cost_per_chunk_duration.replica_count(number_of_ingress_proofs_total)?;

    // Calculate base network fee using current EMA price
    let base_fee = cost_with_replicas.base_network_fee(
        U256::from(bytes_to_store),
        config.chunk_size,
        ema_price,
    )?;

    // Ensure minimum fee in USD terms
    // Convert minimum USD to IRYS tokens using the current EMA price
    let min_fee_irys = config.minimum_term_fee_usd.base_network_fee(
        U256::from(config.chunk_size), // 1 chunk worth
        config.chunk_size,
        ema_price,
    )?;

    if base_fee.amount < min_fee_irys.amount {
        Ok(min_fee_irys.amount)
    } else {
        Ok(base_fee.amount)
    }
}

/// Calculate term fee for temporary storage using ConsensusConfig
/// Uses the same replica count as permanent storage but for submit_ledger_epoch_length duration
pub fn calculate_term_fee_from_config(
    bytes_to_store: u64,
    config: &ConsensusConfig,
    number_of_ingress_proofs_total: u64,
    ema_price: Amount<(IrysPrice, Usd)>,
    timestamp: UnixTimestamp,
) -> Result<U256> {
    calculate_term_fee(
        bytes_to_store,
        config.epoch.submit_ledger_epoch_length,
        config,
        number_of_ingress_proofs_total,
        ema_price,
        timestamp,
    )
}

/// Calculate permanent storage fee including base network fee and ingress proof rewards
pub fn calculate_perm_fee_from_config(
    bytes_to_store: u64,
    config: &ConsensusConfig,
    number_of_ingress_proofs_total: u64,
    ema_price: Amount<(IrysPrice, Usd)>,
    term_fee: U256,
    timestamp: UnixTimestamp,
) -> Result<Amount<(NetworkFee, Irys)>> {
    let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch_at(timestamp)?;

    // Calculate epochs for storage duration
    let epochs_for_storage = config.safe_minimum_number_of_years * config.epochs_per_year();

    // Convert annual decay rate to per-epoch decay rate
    // The decay_rate is annual (e.g., 0.01 = 1% per year) stored in PRECISION_SCALE
    // We need to convert it to per-epoch: decay_per_epoch = decay_annual / epochs_per_year
    let epochs_per_year = U256::from(config.epochs_per_year());
    let decay_rate_per_epoch = Amount::new(safe_div(config.decay_rate.amount, epochs_per_year)?);

    // Apply decay over storage duration
    let cost_per_chunk_duration_adjusted = cost_per_chunk_per_epoch
        .cost_per_replica(epochs_for_storage, decay_rate_per_epoch)?
        .replica_count(number_of_ingress_proofs_total)?;

    // Calculate base network fee
    let base_network_fee = cost_per_chunk_duration_adjusted.base_network_fee(
        U256::from(bytes_to_store),
        config.chunk_size,
        ema_price,
    )?;

    // Add ingress proof rewards to the base network fee
    let total_perm_fee = base_network_fee.add_ingress_proof_rewards(
        term_fee,
        number_of_ingress_proofs_total,
        config.immediate_tx_inclusion_reward_percent,
    )?;

    Ok(total_perm_fee)
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

/// safe addition that errors on overflow
pub fn safe_add(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_add(rhs).ok_or_else(|| eyre!("overflow in add"))
}

/// safe subtraction that errors on underflow
pub fn safe_sub(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_sub(rhs)
        .ok_or_else(|| eyre!("underflow in sub"))
}

/// safe multiplication that errors on overflow
pub fn safe_mul(lhs: U256, rhs: U256) -> Result<U256> {
    lhs.checked_mul(rhs).ok_or_else(|| eyre!("overflow in mul"))
}

/// safe division that errors on division-by-zero
pub fn safe_div(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("division by zero"))
    } else {
        Ok(lhs.checked_div(rhs).unwrap())
    }
}
pub fn safe_mod(lhs: U256, rhs: U256) -> Result<U256> {
    if rhs.is_zero() {
        Err(eyre!("module by zero"))
    } else {
        Ok(lhs % rhs)
    }
}

/// computes (a * b) / c in 256-bit arithmetic with checks.
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
    use crate::UnixTimestamp;
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
            let decay_rate_per_year = Amount::percentage(dec!(0.01)).unwrap(); // 1% per period
            let periods = 200;

            // Action
            let total_cost = cost_per_period
                .cost_per_replica(periods, decay_rate_per_year)?
                .replica_count(1)?;

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
        // r = 0 => no decay, cost is simply epochs * cost_per_epoch
        fn test_zero_decay_rate() -> Result<()> {
            // cost_per_epoch = 1000 (scaled 1e18)
            // decay = 0 => no decay over time
            let cost_per_epoch = Amount::token(dec!(1000)).unwrap();
            let decay = Amount::percentage(dec!(0)).unwrap();
            let epochs = 10;

            let result = cost_per_epoch.cost_per_replica(epochs, decay)?;

            // With zero decay, total = cost_per_epoch * epochs = 1000 * 10 = 10000
            let expected = dec!(10000);
            let actual = result.token_to_decimal()?;
            assert_eq!(
                actual, expected,
                "With zero decay, cost should be epochs * cost_per_epoch"
            );
            Ok(())
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

            let total = cost_per_epoch
                .cost_per_replica(epochs, decay)?
                .replica_count(1)?;

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

            let total = cost_per_epoch
                .cost_per_replica(epochs, decay)?
                .replica_count(1)?;

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
            let network_fee = cost_per_chunk_adjusted.base_network_fee(
                U256::from(bytes_to_store),
                chunk_size,
                price_irys,
            )?;
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

    mod term_fee_calculations {
        use super::*;
        use crate::ConsensusConfig;
        use rust_decimal_macros::dec;

        #[test]
        fn test_term_fee_single_chunk() -> Result<()> {
            // Setup: Use testing config as baseline
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let chunk_size = config.chunk_size; // 256 KB
            let bytes_to_store = chunk_size; // Single chunk
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate term fee
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Convert to decimal for verification
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // The minimum fee is $0.01 USD
            // With IRYS at $1, minimum fee should be 0.01 IRYS
            let expected_min = dec!(0.01);

            assert!(
                term_fee_dec >= expected_min,
                "Term fee {} should be at least minimum fee {}",
                term_fee_dec,
                expected_min
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_16tb() -> Result<()> {
            // Setup: 16TB = 16 * 1024^4 bytes
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let tb_in_bytes = 1024_u64.pow(4);
            let bytes_to_store = 16 * tb_in_bytes; // 16TB
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate term fee
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Convert to decimal for verification
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // Golden data calculation for 16TB with testing config:
            // - 16TB = 17,592,186,044,416 bytes = 67,108,864 chunks (at 256KB/chunk)
            // - Annual cost: $0.01/GB/year
            // - Cost per chunk per epoch: ~$0.0000000000929
            // - Duration: 5 epochs (submit_ledger_epoch_length)
            // - Replicas: 10 (number_of_ingress_proofs)
            // - No decay for term storage (0% decay rate)
            //
            // Expected cost = 67,108,864 chunks * $0.0000000000929 * 5 epochs * 10 replicas
            //               = $0.3117199391 USD
            // With IRYS at $1, cost = 0.3117199391 IRYS
            let expected_fee = dec!(0.3117199391);

            // Allow small tolerance for floating point precision
            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.0000001),
                "16TB term fee should be {} IRYS, got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_below_minimum() -> Result<()> {
            // Setup: Create a config with very low cost to trigger minimum fee
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            config.annual_cost_per_gb = Amount::token(dec!(0.0000001))?; // Very low cost

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = 1; // Just 1 byte
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate term fee
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Convert to decimal
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // Should be exactly the minimum fee ($0.01 with IRYS at $1)
            let expected = dec!(0.01);

            assert_eq!(
                term_fee_dec, expected,
                "Fee below minimum should be rounded up to minimum"
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_with_different_prices() -> Result<()> {
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = config.chunk_size; // 1 chunk

            // Test with different IRYS prices
            let prices = vec![
                (dec!(0.10), dec!(0.10)),   // IRYS at $0.10, min fee = 0.10 IRYS
                (dec!(1.00), dec!(0.01)),   // IRYS at $1.00, min fee = 0.01 IRYS
                (dec!(10.00), dec!(0.001)), // IRYS at $10.00, min fee = 0.001 IRYS
            ];

            for (price_usd, expected_min_irys) in prices {
                let irys_price = Amount::token(price_usd)?;
                let term_fee = calculate_term_fee_from_config(
                    bytes_to_store,
                    &config,
                    number_of_ingress_proofs_total,
                    irys_price,
                    UnixTimestamp::from_secs(0),
                )?;
                let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

                // Minimum fee is $0.01 USD, so in IRYS tokens = 0.01 / price_usd
                assert!(
                    (term_fee_dec - expected_min_irys).abs() < dec!(0.0000001),
                    "With IRYS at ${}, minimum fee should be {} IRYS, got {}",
                    price_usd,
                    expected_min_irys,
                    term_fee_dec
                );
            }

            Ok(())
        }

        #[test]
        fn test_term_fee_1tb() -> Result<()> {
            // Setup: 1TB - common user scenario that's above minimum fee
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let tb_in_bytes = 1024_u64.pow(4); // 1TB = 1,099,511,627,776 bytes
            let bytes_to_store = tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate term fee
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // Golden data: 1TB = 4,194,304 chunks
            // With testing config: 12s blocks, 100 blocks/epoch = 1200s/epoch
            // Epochs per year = 365*24*60*60 / 1200 = 26280
            // Cost per chunk per epoch = 0.01 / 4096 / 26280 = 9.292532e-11
            // Term cost = 4194304 * 9.292532e-11 * 5 * 10 = 0.0194824961523712
            let expected_fee = dec!(0.0194824961523712);

            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.0000000001),
                "1TB term fee should be {} IRYS, got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_1pb_extreme() -> Result<()> {
            // Setup: 1PB - extreme case to test large numbers
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let pb_in_bytes = 1024_u64.pow(5); // 1PB = 1,125,899,906,842,624 bytes
            let bytes_to_store = pb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate term fee
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // Golden data: 1PB = 4,294,967,296 chunks
            // Cost = 4,294,967,296 * $0.0000000000929 * 5 epochs * 10 replicas
            //      = $19.9500761035 USD
            let expected_fee = dec!(19.9500761035);

            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.0000001),
                "1PB term fee should be {} IRYS, got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            // Verify it's still reasonable and didn't overflow
            assert!(
                term_fee_dec < dec!(100000),
                "1PB fee should be reasonable, not astronomical"
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_chunk_boundaries() -> Result<()> {
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let chunk_size = config.chunk_size;
            let irys_price = Amount::token(dec!(1.0))?;

            // Use 10 million chunks to be well above minimum fee ($0.01)
            // 10 million chunks = ~2.5TB, cost ~$0.046
            let bytes_10m_chunks = 10_000_000 * chunk_size;
            let fee_10m = calculate_term_fee_from_config(
                bytes_10m_chunks,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let fee_10m_dec = Amount::<Irys>::new(fee_10m).token_to_decimal()?;

            // Test 10 million chunks + 1 byte (should round up to 10,000,001 chunks)
            let bytes_10m_plus_1 = (10_000_000 * chunk_size) + 1;
            let fee_10m_plus = calculate_term_fee_from_config(
                bytes_10m_plus_1,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let fee_10m_plus_dec = Amount::<Irys>::new(fee_10m_plus).token_to_decimal()?;

            // The difference should be exactly the cost of 1 chunk
            let expected_ratio = dec!(10000001) / dec!(10000000);
            let actual_ratio = fee_10m_plus_dec / fee_10m_dec;
            let ratio_diff = (actual_ratio - expected_ratio).abs();

            assert!(
                ratio_diff < dec!(0.0000001),
                "Adding 1 byte should increase cost by exactly 1 chunk. Expected ratio {}, got {}",
                expected_ratio,
                actual_ratio
            );

            // Verify that we're above minimum fee
            assert!(
                fee_10m_dec > dec!(0.04),
                "Test should use enough chunks to be above minimum fee, got {}",
                fee_10m_dec
            );

            Ok(())
        }

        #[test]
        fn test_extreme_token_prices() -> Result<()> {
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let bytes_to_store = 100 * config.chunk_size; // 100 chunks

            // Test with very cheap IRYS ($0.001)
            let cheap_price = Amount::token(dec!(0.001))?;
            let cheap_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                cheap_price,
                UnixTimestamp::from_secs(0),
            )?;
            let cheap_fee_dec = Amount::<Irys>::new(cheap_fee).token_to_decimal()?;

            // Test with very expensive IRYS ($1000)
            let expensive_price = Amount::token(dec!(1000))?;
            let expensive_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                expensive_price,
                UnixTimestamp::from_secs(0),
            )?;
            let expensive_fee_dec = Amount::<Irys>::new(expensive_fee).token_to_decimal()?;

            // The fees in IRYS should be inversely proportional to price
            // cheap_fee * 0.001 = expensive_fee * 1000 (both in USD)
            let cheap_usd = cheap_fee_dec * dec!(0.001);
            let expensive_usd = expensive_fee_dec * dec!(1000);
            let usd_diff = (cheap_usd - expensive_usd).abs();

            assert!(
                usd_diff < dec!(0.0000001),
                "USD value should be the same regardless of token price. Got {} vs {}",
                cheap_usd,
                expensive_usd
            );

            // At $0.001/IRYS, minimum $0.01 = 10 IRYS
            assert!(
                cheap_fee_dec >= dec!(10),
                "At $0.001/IRYS, minimum should be 10 IRYS"
            );

            // At $1000/IRYS, minimum $0.01 = 0.00001 IRYS
            let expected_min_expensive = dec!(0.00001);
            if expensive_fee_dec <= expected_min_expensive * dec!(2) {
                // If we're near minimum, verify it's correct
                assert!(
                    (expensive_fee_dec - expected_min_expensive).abs() < dec!(0.0000001),
                    "At $1000/IRYS, minimum should be 0.00001 IRYS"
                );
            }

            Ok(())
        }

        #[test]
        fn test_spreadsheet_golden_data_1tb_1epoch() -> Result<()> {
            // Golden data from pricing spreadsheet:
            // - Total Fee Per Epoch Storage Price (TB): $0.0753
            // - 1TB @ 1 epoch = $0.0753

            // To match the spreadsheet, we need to reverse-engineer the annual cost
            // Spreadsheet shows $0.0753 per TB per epoch with 10 replicas
            // This means $0.00753 per TB per epoch per replica

            // Create custom config matching spreadsheet economics
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            // Calculate the required annual_cost_per_gb to achieve $0.0753/TB/epoch
            // With testing config: 26280 epochs/year
            // $0.00753 per TB per epoch per replica = $197.9064 per TB per year per replica
            // $197.9064 per TB per year = $0.193245 per GB per year
            config.annual_cost_per_gb = Amount::token(dec!(0.193245))?;

            // Term storage: 1 epoch (not 5), 10 replicas, 0% decay
            let tb_in_bytes = 1024_u64.pow(4); // 1TB
            let bytes_to_store = tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate for 1 epoch (not using the standard term fee function which uses 5 epochs)
            let epochs_for_storage = 1_u64;
            let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch()?;
            let decay_rate = Amount::percentage(dec!(0.0))?; // No decay for term storage

            let cost_per_chunk_adjusted = cost_per_chunk_per_epoch
                .cost_per_replica(epochs_for_storage, decay_rate)?
                .replica_count(number_of_ingress_proofs_total)?;

            let term_fee = cost_per_chunk_adjusted.base_network_fee(
                U256::from(bytes_to_store),
                config.chunk_size,
                irys_price,
            )?;

            let term_fee_dec = Amount::<Irys>::new(term_fee.amount).token_to_decimal()?;

            // Expected from spreadsheet: $0.0753 for 1TB @ 1 epoch
            let expected_fee = dec!(0.0753);

            // Allow reasonable tolerance for rounding differences
            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.0001),
                "1TB @ 1 epoch should be {} IRYS (spreadsheet value), got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_spreadsheet_golden_data_1tb_5epochs() -> Result<()> {
            // Golden data from pricing spreadsheet:
            // - 1TB @ 5 epochs = $0.3767

            // Create custom config matching spreadsheet economics
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            // Use the same annual cost that achieves $0.0753/TB/epoch
            config.annual_cost_per_gb = Amount::token(dec!(0.193245))?;

            // Term storage: 5 epochs, 10 replicas, 0% decay
            let tb_in_bytes = 1024_u64.pow(4); // 1TB
            let bytes_to_store = tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Use the standard term fee calculation (5 epochs)
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;

            // Expected from spreadsheet: $0.3767 for 1TB @ 5 epochs
            let expected_fee = dec!(0.3767);

            // Allow reasonable tolerance for rounding differences
            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.0003),
                "1TB @ 5 epochs should be {} IRYS (spreadsheet value), got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_spreadsheet_golden_data_per_gb() -> Result<()> {
            // Golden data from pricing spreadsheet:
            // - Per GB pricing should be $0.00007358 per epoch

            // Create custom config matching spreadsheet economics
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            // Use annual cost that gives us the target per-GB price
            // $0.00007358 per GB per epoch with 10 replicas
            // = $0.000007358 per GB per epoch per replica
            // With 26280 epochs/year: $0.193368 per GB per year
            config.annual_cost_per_gb = Amount::token(dec!(0.193368))?;

            let gb_in_bytes = 1024_u64.pow(3); // 1GB
            let bytes_to_store = gb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate for 1 epoch
            let epochs_for_storage = 1_u64;
            let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch()?;
            let decay_rate = Amount::percentage(dec!(0.0))?; // No decay for term storage

            let cost_per_chunk_adjusted = cost_per_chunk_per_epoch
                .cost_per_replica(epochs_for_storage, decay_rate)?
                .replica_count(number_of_ingress_proofs_total)?;

            let term_fee = cost_per_chunk_adjusted.base_network_fee(
                U256::from(bytes_to_store),
                config.chunk_size,
                irys_price,
            )?;

            let term_fee_dec = Amount::<Irys>::new(term_fee.amount).token_to_decimal()?;

            // Expected from spreadsheet: $0.00007358 per GB per epoch with 10 replicas
            // This is the total cost for 1GB for 1 epoch across all replicas
            let expected_fee = dec!(0.00007358);

            // Allow reasonable tolerance for rounding differences
            let diff = (term_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.00000001),
                "1GB @ 1 epoch should be {} IRYS (spreadsheet value), got {} IRYS (diff: {})",
                expected_fee,
                term_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_spreadsheet_golden_data_daily_cost() -> Result<()> {
            // Golden data from pricing spreadsheet:
            // - Daily Cost per TB: $0.0075 (without replicas)
            // - This translates to specific epoch costs based on epoch duration

            // Create custom config matching spreadsheet economics
            let mut config = ConsensusConfig::testing();

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            // Daily cost per TB (single replica) = $0.0075
            // Annual cost = $0.0075 * 365 = $2.7375 per TB
            // = $0.00267333984375 per GB per year
            config.annual_cost_per_gb = Amount::token(dec!(0.00267333984375))?;

            // Calculate daily cost per TB (single replica)
            // Annual cost per TB = $2.75
            // Daily cost per TB = $2.75 / 365 = $0.00753424657...

            let tb_in_bytes = 1024_u64.pow(4); // 1TB
            let bytes_to_store = tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Calculate epochs per day
            // With 12s blocks and 100 blocks/epoch = 1200s/epoch = 20 minutes/epoch
            // 24 hours = 1440 minutes = 72 epochs per day
            let epochs_per_day = 72_u64;

            let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch()?;
            let decay_rate = Amount::percentage(dec!(0.0))?; // No decay for term storage

            // Calculate for 72 epochs (1 day) with 1 replica
            let cost_per_chunk_adjusted = cost_per_chunk_per_epoch
                .cost_per_replica(epochs_per_day, decay_rate)?
                .replica_count(1)?; // Single replica to match spreadsheet daily cost

            let daily_fee = cost_per_chunk_adjusted.base_network_fee(
                U256::from(bytes_to_store),
                config.chunk_size,
                irys_price,
            )?;

            let daily_fee_dec = Amount::<Irys>::new(daily_fee.amount).token_to_decimal()?;

            // Expected daily cost from spreadsheet: $0.0075 per TB (single replica)
            let expected_fee = dec!(0.0075);

            // Allow reasonable tolerance for rounding differences
            let diff = (daily_fee_dec - expected_fee).abs();
            assert!(
                diff < dec!(0.00001),
                "1TB daily cost (single replica) should be close to {} IRYS, got {} IRYS (diff: {})",
                expected_fee,
                daily_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_dynamic_epoch_count() -> Result<()> {
            // Test that the new calculate_term_fee function correctly uses different epoch counts
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = 1024_u64.pow(4) * 100; // 100TB - large enough to avoid minimum fee
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token

            // Test with different epoch counts
            let test_cases = vec![
                (1_u64, "1 epoch"),
                (3_u64, "3 epochs"),
                (5_u64, "5 epochs (default)"),
                (10_u64, "10 epochs"),
                (100_u64, "100 epochs"),
            ];

            let mut previous_fee = U256::zero();
            for (epochs, description) in test_cases {
                let fee = calculate_term_fee(
                    bytes_to_store,
                    epochs,
                    &config,
                    number_of_ingress_proofs_total,
                    irys_price,
                    UnixTimestamp::from_secs(0),
                )?;

                // Fee should increase with more epochs
                assert!(
                    fee > previous_fee || epochs == 1,
                    "Fee for {} should be greater than previous: {} vs {}",
                    description,
                    fee,
                    previous_fee
                );

                // Verify the fee is proportional to epochs (no decay for term storage)
                if epochs == 1 {
                    let fee_1_epoch = fee;
                    let fee_5_epochs = calculate_term_fee(
                        bytes_to_store,
                        5,
                        &config,
                        number_of_ingress_proofs_total,
                        irys_price,
                        UnixTimestamp::from_secs(0),
                    )?;
                    let fee_10_epochs = calculate_term_fee(
                        bytes_to_store,
                        10,
                        &config,
                        number_of_ingress_proofs_total,
                        irys_price,
                        UnixTimestamp::from_secs(0),
                    )?;

                    // With no decay, 5 epochs should cost ~5x one epoch
                    let ratio_5 = fee_5_epochs / fee_1_epoch;
                    assert!(
                        ratio_5 >= U256::from(4) && ratio_5 <= U256::from(6),
                        "5 epochs should cost ~5x one epoch, got ratio: {}",
                        ratio_5
                    );

                    // With no decay, 10 epochs should cost ~10x one epoch
                    let ratio_10 = fee_10_epochs / fee_1_epoch;
                    assert!(
                        ratio_10 >= U256::from(9) && ratio_10 <= U256::from(11),
                        "10 epochs should cost ~10x one epoch, got ratio: {}",
                        ratio_10
                    );
                }

                previous_fee = fee;
            }

            Ok(())
        }

        #[test]
        fn test_dynamic_epoch_vs_config() -> Result<()> {
            // Verify that calculate_term_fee with config epochs matches calculate_term_fee_from_config
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = 1024_u64.pow(3) * 500; // 500GB
            let irys_price = Amount::token(dec!(2.5))?; // $2.50 per IRYS token

            // Calculate using the old function
            let fee_from_config = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Calculate using the new function with same epoch count
            let fee_dynamic = calculate_term_fee(
                bytes_to_store,
                config.epoch.submit_ledger_epoch_length,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Should be exactly the same
            assert_eq!(
                fee_from_config, fee_dynamic,
                "Dynamic function should match config function when using same epochs"
            );

            Ok(())
        }

        #[test]
        fn test_zero_epochs() -> Result<()> {
            // Test edge case: 0 epochs should still charge minimum fee
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = config.chunk_size; // 1 chunk
            let irys_price = Amount::token(dec!(1.0))?;

            let fee = calculate_term_fee(
                bytes_to_store,
                0,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let fee_dec = Amount::<Irys>::new(fee).token_to_decimal()?;

            // Should be exactly the minimum fee ($0.01 with IRYS at $1)
            let expected_min = dec!(0.01);
            assert_eq!(
                fee_dec, expected_min,
                "Zero epochs should still charge minimum fee"
            );

            Ok(())
        }

        #[test]
        fn test_very_large_epoch_count() -> Result<()> {
            // Test that large epoch counts don't cause overflow
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            config.difficulty_adjustment.block_time = 12;
            config.chunk_size = 262144;
            config.num_chunks_in_partition = 51872000;

            let bytes_to_store = 1024_u64.pow(3); // 1GB
            let irys_price = Amount::token(dec!(1.0))?;

            // Test with a year's worth of epochs (~26280 epochs)
            let epochs_per_year = config.epochs_per_year();
            let fee = calculate_term_fee(
                bytes_to_store,
                epochs_per_year,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;
            let fee_dec = Amount::<Irys>::new(fee).token_to_decimal()?;

            // Should be reasonable (not astronomical due to overflow)
            assert!(
                fee_dec < dec!(1000),
                "Fee for 1GB for 1 year should be reasonable, got {}",
                fee_dec
            );

            // Should be more than minimum fee of $0.01
            assert!(
                fee_dec > dec!(0.01),
                "Fee for 1GB for 1 year should be more than minimum, got {}",
                fee_dec
            );

            Ok(())
        }

        #[test]
        fn test_term_fee_higher_post_cascade() -> Result<()> {
            use crate::hardfork_config::Cascade;

            let mut config = ConsensusConfig::testing();
            // Disable minimum fee floor so we see the raw cost difference
            config.minimum_term_fee_usd = Amount::token(dec!(0))?;
            config.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(1000),
                one_year_epoch_length: 365,
                thirty_day_epoch_length: 30,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
            // Use large data to avoid any rounding effects
            let bytes = 100_000 * config.chunk_size;
            let irys_price = Amount::token(dec!(1.0))?;

            let pre_ts = UnixTimestamp::from_secs(999);
            let post_ts = UnixTimestamp::from_secs(1000);
            let fee_pre = calculate_term_fee(bytes, 5, &config, 1, irys_price, pre_ts)?;
            let fee_post = calculate_term_fee(bytes, 5, &config, 1, irys_price, post_ts)?;

            // Cascade default $0.028 vs base $0.01 → 2.8x increase
            let pre_dec = Amount::<Irys>::new(fee_pre).token_to_decimal()?;
            let post_dec = Amount::<Irys>::new(fee_post).token_to_decimal()?;
            let ratio = post_dec / pre_dec;
            assert!(
                ratio > dec!(2.7) && ratio < dec!(2.9),
                "post-Cascade term fee should be ~2.8x pre-Cascade, got ratio {}",
                ratio
            );

            Ok(())
        }
    }

    mod perm_fee_calculations {
        use super::*;
        use crate::ConsensusConfig;
        use rust_decimal_macros::dec;

        #[test]
        fn test_perm_fee_16tb() -> Result<()> {
            // Setup: 16TB - same as term test for comparison
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            config.difficulty_adjustment.block_time = 12;
            // config.difficulty_adjustment.difficulty_adjustment_interval = 100800000;
            config.chunk_size = 262144;
            // config.num_chunks_in_partition = 51872000;

            let tb_in_bytes = 1024_u64.pow(4);
            let bytes_to_store = 16 * tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?; // $1 per IRYS token
                                                        // First calculate term fee (needed for perm fee calculation)
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Calculate permanent fee
            let perm_fee = calculate_perm_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                term_fee,
                UnixTimestamp::from_secs(0),
            )?;
            let perm_fee_dec = perm_fee.token_to_decimal()?;

            // Golden data: 16TB = 67,108,864 chunks
            // Base cost: $141,666.6756358678
            // Ingress rewards: $0.1558599696
            // Total: $141,666.8314958374 USD
            let expected_total = dec!(141666.8314958374);

            let diff = (perm_fee_dec - expected_total).abs();
            assert!(
                diff < dec!(1), // Allow $1 tolerance for large numbers
                "16TB perm fee should be {} IRYS, got {} IRYS (diff: {})",
                expected_total,
                perm_fee_dec,
                diff
            );

            // Verify it's massively more expensive than term
            let term_fee_dec = Amount::<Irys>::new(term_fee).token_to_decimal()?;
            let ratio = perm_fee_dec / term_fee_dec;
            assert!(
                ratio > dec!(400000),
                "Permanent should be >400,000x more expensive than term, got {}x",
                ratio
            );

            Ok(())
        }

        #[test]
        fn test_perm_fee_1tb() -> Result<()> {
            // Setup: 1TB - reasonable size for permanent storage
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let tb_in_bytes = 1024_u64.pow(4);
            let bytes_to_store = tb_in_bytes;
            let irys_price = Amount::token(dec!(1.0))?;

            // Calculate term fee first
            let term_fee = calculate_term_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                UnixTimestamp::from_secs(0),
            )?;

            // Calculate permanent fee
            let perm_fee = calculate_perm_fee_from_config(
                bytes_to_store,
                &config,
                number_of_ingress_proofs_total,
                irys_price,
                term_fee,
                UnixTimestamp::from_secs(0),
            )?;
            let perm_fee_dec = perm_fee.token_to_decimal()?;

            // Golden data: 1TB = 4,194,304 chunks
            // With 200 years and 1% decay, and 26280 epochs/year:
            // Decay factor ≈ 2,272,339
            // Base cost: $8854.1672272417
            // Term fee: $0.0194824962
            // Ingress rewards: $0.0097412481
            // Total: $8854.1769684898 USD
            let expected_total = dec!(8854.1769684898);

            let diff = (perm_fee_dec - expected_total).abs();
            assert!(
                diff < dec!(1),
                "1TB perm fee should be {} IRYS, got {} IRYS (diff: {})",
                expected_total,
                perm_fee_dec,
                diff
            );

            Ok(())
        }

        #[test]
        fn test_perm_fee_higher_post_cascade() -> Result<()> {
            use crate::hardfork_config::Cascade;

            let mut config = ConsensusConfig::testing();
            config.minimum_term_fee_usd = Amount::token(dec!(0)).unwrap();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            config.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(1000),
                one_year_epoch_length: 365,
                thirty_day_epoch_length: 30,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
            let proofs = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let bytes = 100_000 * config.chunk_size;
            let irys_price = Amount::token(dec!(1.0))?;

            let pre_ts = UnixTimestamp::from_secs(999);
            let post_ts = UnixTimestamp::from_secs(1000);

            let term_pre =
                calculate_term_fee_from_config(bytes, &config, proofs, irys_price, pre_ts)?;
            let perm_pre = calculate_perm_fee_from_config(
                bytes, &config, proofs, irys_price, term_pre, pre_ts,
            )?;

            let term_post =
                calculate_term_fee_from_config(bytes, &config, proofs, irys_price, post_ts)?;
            let perm_post = calculate_perm_fee_from_config(
                bytes, &config, proofs, irys_price, term_post, post_ts,
            )?;

            assert!(
                perm_post.amount > perm_pre.amount,
                "perm fee post-Cascade ({}) should exceed pre-Cascade ({})",
                perm_post.amount,
                perm_pre.amount
            );

            Ok(())
        }

        #[test]
        fn test_perm_fee_decay_calculation() -> Result<()> {
            // Test the decay formula explicitly
            let mut config = ConsensusConfig::testing();
            config.hardforks.frontier.number_of_ingress_proofs_total = 10;
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));

            // Use a large size to avoid minimum fee effects
            let bytes_to_store = 10000 * config.chunk_size; // 10000 chunks
            let irys_price = Amount::token(dec!(1.0))?;

            // Calculate the base storage cost (without ingress rewards)
            let epochs_for_storage = config.years_to_epochs(config.safe_minimum_number_of_years);
            let cost_per_chunk_per_epoch = config.cost_per_chunk_per_epoch()?;

            // Calculate with decay (convert annual decay to per-epoch)
            let epochs_per_year = U256::from(config.epochs_per_year());
            let decay_rate_per_epoch =
                Amount::new(safe_div(config.decay_rate.amount, epochs_per_year)?);
            let cost_with_decay = cost_per_chunk_per_epoch
                .cost_per_replica(epochs_for_storage, decay_rate_per_epoch)?
                .replica_count(number_of_ingress_proofs_total)?;
            let base_fee_with_decay = cost_with_decay.base_network_fee(
                U256::from(bytes_to_store),
                config.chunk_size,
                irys_price,
            )?;

            // Calculate without decay (decay_rate_per_year =  0)
            let cost_no_decay = cost_per_chunk_per_epoch
                .cost_per_replica(epochs_for_storage, Amount::percentage(dec!(0))?)?
                .replica_count(number_of_ingress_proofs_total)?;
            let base_fee_no_decay = cost_no_decay.base_network_fee(
                U256::from(bytes_to_store),
                config.chunk_size,
                irys_price,
            )?;

            let decay_amount = base_fee_with_decay.token_to_decimal()?;
            let no_decay_amount = base_fee_no_decay.token_to_decimal()?;
            let ratio = decay_amount / no_decay_amount;

            // With 1% decay over 200 years, the decay factor is approximately 0.432
            // (1 - (1-0.01/26298)^5259600) / (0.01/26298) / 5259600 ≈ 0.432
            assert!(
                ratio > dec!(0.43) && ratio < dec!(0.44),
                "Decay should reduce cost to ~43.2% of no-decay. Got ratio: {}",
                ratio
            );

            Ok(())
        }
    }
}
