//! A utility crate fro calculating netwokrk fees, costs for storing different amounts of data, and EMA for blocks.
//!
//! Core data types:
//! - `Amount<(CostPerGb, Usd)>` - Cost in $USD of storing 1GB on irys (per single replica), data part of the config
//! - `Amount<(IrysPrice, Usd)>` - Cost in $USD of a single $IRYS token, the data retrieved form oracles
//! - `Amount<(Ema, Usd)>` - Exponential Moving Average for a single $IRYS token, the data to be stored in blocks
//! - `Amount<(NetworkFee, Irys)>` - The cost in $IRYS that the user will have to pay to store his data on Irys
use core::{fmt::Debug, marker::PhantomData, ops::Deref};

pub use arbitrary::{Arbitrary, Unstructured};
use eyre::{ensure, OptionExt as _};
pub use phantoms::*;
pub use reth_codecs::Compact;
pub use rust_decimal;
use rust_decimal::{prelude::FromPrimitive, Decimal, MathematicalOps as _};
pub use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// A wrapper around [`rust_decimal::Decimal`] that also denominates the value in currency using [`PhantomData`]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Arbitrary, Default,
)]
pub struct Amount<T> {
    #[arbitrary(with = arbitrary_rust_decimal)]
    amount: Decimal,
    _t: PhantomData<T>,
}

impl<T> Amount<T> {
    pub const SERIALIZED_SIZE: usize = 16;

    /// Initialize a new Amount; `PhantomData` to be inferred from usage
    #[must_use]
    pub const fn new(amount: Decimal) -> Self {
        Self {
            amount,
            _t: PhantomData,
        }
    }
}

impl<T> Deref for Amount<T> {
    type Target = Decimal;

    fn deref(&self) -> &Self::Target {
        &self.amount
    }
}

mod phantoms {
    use arbitrary::Arbitrary;

    /// The cost of storing a single GB of data.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct CostPerGb;

    /// Currency denomintator util type.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct Usd;

    /// Currency denominator util type.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct Irys;

    /// Exponential Moving Average.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct Ema;

    /// Decay rate to account for storage hardware getting cheaper.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct DecayRate;

    /// The network fee, that the user would have to pay for storing his data on Irys.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct NetworkFee;

    /// Price of the $IRYS token.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct IrysPrice;

    /// Cost per storing 1GB, of data. Includes adjustment for storage duration.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Arbitrary, Default)]
    pub struct CostPerGbYearAdjusted;
}

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
        years_to_pay_for_storage: u64,
        decay_rate: Amount<DecayRate>,
    ) -> eyre::Result<Amount<(CostPerGbYearAdjusted, Usd)>> {
        let annual_cost_per_byte = self.amount;

        // (1 - r)^n
        let one_minus_r_pow = (Decimal::ONE.saturating_sub(*decay_rate))
            .checked_powu(years_to_pay_for_storage)
            .ok_or_else(|| eyre::eyre!("too many years to pay for: {years_to_pay_for_storage}"))?;

        // fraction = [ 1 - (1-r)^n ] / r
        let fraction = (Decimal::ONE.saturating_sub(one_minus_r_pow))
            .checked_div(decay_rate.amount)
            .ok_or_else(|| eyre::eyre!("decay rate invalid {decay_rate}"))?;

        // total = annual_cost * fraction
        let total = annual_cost_per_byte
            .checked_mul(fraction)
            .ok_or_else(|| eyre::eyre!("fraction too large: fraction={fraction} annual_cost_per_byte={annual_cost_per_byte}"))?;

        Ok(Amount {
            amount: total,
            _t: PhantomData,
        })
    }
}

impl Amount<(CostPerGbYearAdjusted, Usd)> {
    /// Apply a multiplier of how much would storing the data cost for `n` replicas.
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    pub fn replica_count(self, replicas: u64) -> eyre::Result<Self> {
        let amount = self.amount.checked_mul(replicas.into()).ok_or_else(|| {
            eyre::eyre!("overflow during replica multiplication {replicas}, cost={self}")
        })?;
        Ok(Self { amount, ..self })
    }

    /// calculate the network fee, denominated in $IRYS tokens
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    pub fn base_network_fee(
        self,
        bytes_to_store: Decimal,
        irys_token_price: Amount<(IrysPrice, Usd)>,
    ) -> eyre::Result<Amount<(NetworkFee, Irys)>> {
        // divide by the ratio
        let bytes_in_gb = dec!(1_073_741_824); // 1024 * 1024 * 1024
        let price_ratio = bytes_to_store
            .checked_div(bytes_in_gb)
            .ok_or_else(|| eyre::eyre!("invalid byte amount to store {bytes_to_store}"))?;

        // annual cost per byte in usd
        let usd_fee = self
            .amount
            .checked_mul(price_ratio)
            .ok_or_else(|| eyre::eyre!("price ratio causes overflow {price_ratio}, cost={self}"))?;

        // converted to $IRYS
        let network_fee = usd_fee
            .checked_div(*irys_token_price)
            .ok_or_eyre("invalid irys token price")?;

        Ok(Amount {
            amount: network_fee,
            _t: PhantomData,
        })
    }
}

impl Amount<(NetworkFee, Irys)> {
    /// Add additional network fee for storing data to increase incentivisation.
    ///
    /// 0.05 => 5%
    /// 1.00 => 100%
    /// 0.50 => 50%
    ///
    /// # Errors
    ///
    /// Whenever any of the math operations fail due to bounds checks.
    pub fn add_multiplier(self, percentage: Decimal) -> eyre::Result<Self> {
        // amount * (100% + x%)
        let amount = self
            .amount
            .checked_mul(
                percentage
                    .checked_add(dec!(1))
                    .ok_or_else(|| eyre::eyre!("rewarad percentage too large {percentage}"))?,
            )
            .ok_or_else(|| eyre::eyre!("reward percentage too large {percentage}, fee={self}"))?;
        Ok(Self {
            amount,
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
        previous_block_ema: Amount<(Ema, Usd)>,
    ) -> eyre::Result<Amount<(Ema, Usd)>> {
        // Safely convert `total_blocks_in_epoch + 1` to Decimal
        let denominator = Decimal::from(total_past_blocks)
            .checked_add(dec!(1))
            .ok_or_else(|| {
                eyre::eyre!("failed to compute total_past_blocks + 1 {total_past_blocks}")
            })?;

        // Calculate alpha = 2 / (n+1)
        let alpha = dec!(2).checked_div(denominator).ok_or_else(|| {
            eyre::eyre!("failed to compute smoothing factor alpha with denominator {denominator}")
        })?;
        ensure!(
            alpha > dec!(0) || alpha <= dec!(1),
            "computed alpha={alpha} is out of the valid range (0,1]"
        );

        // Calculate (1 - alpha)
        let one_minus_alpha = dec!(1)
            .checked_sub(alpha)
            .ok_or_else(|| eyre::eyre!("failed to compute (1 - {alpha}) "))?;

        // alpha * current_irys_price
        let scaled_current_price = alpha
            .checked_mul(self.amount)
            .ok_or_else(|| eyre::eyre!("failed to multiply {alpha} by {self}"))?;

        // (1 - alpha) * last_block_ema
        let scaled_last_ema = one_minus_alpha
            .checked_mul(*previous_block_ema)
            .ok_or_else(|| {
                eyre::eyre!("failed to multiply (1 - {alpha}) by {previous_block_ema}")
            })?;

        // alpha * price + (1 - alpha) * previous_ema
        let current_block_ema = scaled_current_price
            .checked_add(scaled_last_ema)
            .ok_or_else(|| {
                eyre::eyre!("failed to add {scaled_current_price} price to {scaled_last_ema} EMA")
            })?;

        Ok(Amount::new(current_block_ema))
    }
}

#[expect(clippy::min_ident_chars, reason = "part of the Display interface")]
#[expect(
    clippy::use_debug,
    reason = "Our phantom types don't need display impl"
)]
impl<T> core::fmt::Display for Amount<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "<{:?}>: {}", self._t, self.amount)
    }
}

fn arbitrary_rust_decimal(u: &mut Unstructured<'_>) -> arbitrary::Result<Decimal> {
    let lo = u.int_in_range(0..=u32::MAX)?;
    let mid = u.int_in_range(0..=u32::MAX)?;
    let hi = u.int_in_range(0..=u32::MAX)?;
    let scale = u.int_in_range(0..=28)?;
    let negative = u.arbitrary()?;
    let decimal = Decimal::from_parts(lo, mid, hi, negative, scale);
    Ok(decimal)
}

// compile time assertions
const _: () = {
    // `rust_decimal::Decimal` serializes to 16 byte array
    let decimal_bytes = Decimal::from_parts(1, 1, 1, false, 1).serialize();
    assert!(decimal_bytes.len() == 16);
};

impl<T> Compact for Amount<T> {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // Extract the 128-bit raw parts of the Decimal
        let packed_data = self.amount.serialize();

        // Put them in little-endian form into `buf`
        buf.put(&packed_data[..]);

        // We used 16 bytes total (4 parts * 4 bytes each)
        packed_data.len()
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        assert!(
            len >= Self::SERIALIZED_SIZE,
            "Compact encoding for Decimal requires 16 bytes but received {}",
            len
        );
        let buf_exact = buf[0..Self::SERIALIZED_SIZE]
            .try_into()
            .expect("we validated that we have at least 16 bytes");

        let res = Self {
            amount: Decimal::deserialize(buf_exact),
            _t: PhantomData,
        };
        (res, &buf[Self::SERIALIZED_SIZE..])
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "used in tests")]
#[expect(clippy::panic_in_result_fn, reason = "used in tests")]
mod tests {
    use super::*;
    use eyre::Result;
    use rust_decimal::Decimal;

    mod cost_per_byte {
        use super::*;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup
            let annual = Amount::new(dec!(0.01));
            let dec = Amount::new(dec!(0.01));
            let years = 200;

            // Action
            let cost_per_gb = annual.cost_per_replica(years, dec)?.replica_count(1)?;

            // Assert - cost per gb / single replica
            let expected = Decimal::from_str_exact("0.8661")?;
            let diff = (cost_per_gb.amount - expected).abs();
            assert!(diff < Decimal::from_str_exact("0.0001")?);

            // Assert - cost per gb / 10 replicas
            let cost_per_10_in_gb = cost_per_gb.replica_count(10)?;
            let expected = Decimal::from_str_exact("8.66")?;
            let diff = (cost_per_10_in_gb.amount - expected).abs();
            assert!(diff < Decimal::from_str_exact("0.001")?);
            Ok(())
        }

        #[test]
        // r=0 => division by zero => should Err
        fn test_zero_decay_rate() {
            // setup
            let annual = Amount::new(dec!(1000));
            let dec = Amount::new(dec!(0.0));
            let years = 10;

            // actoin
            let result = annual.cost_per_replica(years, dec);

            // assert
            assert!(result.is_err(), "Expected an error for r=0, got Ok(...)");
        }

        #[test]
        // r=1 => fraction = (1 - (1-1)^n)/1 => (1 - 0^n)/1 => 1
        fn test_full_decay_rate() -> Result<()> {
            // setup
            let annual = Amount::new(dec!(500));
            let dec = Amount::new(dec!(1.0));
            let years_to_pay_for_storage = 5;

            // action
            let total = annual
                .cost_per_replica(years_to_pay_for_storage, dec)
                .unwrap()
                .replica_count(1)?;

            // assert
            assert_eq!(total.amount, Decimal::from(500_u64));
            Ok(())
        }

        #[test]
        fn test_decay_rate_above_one() {
            // setup
            let annual = Amount::new(dec!(0.01));
            let dec = Amount::new(dec!(1.5)); // 150%
            let years = 200;

            // actoin
            let result = annual
                .cost_per_replica(years, dec)
                .unwrap()
                .replica_count(1);

            // assert
            assert!(result.is_ok(), "Expected error for decay > 1.0");
        }

        #[test]
        fn test_no_years_to_pay() -> Result<()> {
            // setup
            let annual = Amount::new(dec!(1234.56));
            let dec = Amount::new(dec!(0.05)); // 5%
            let years = 0;

            // action
            let total = annual
                .cost_per_replica(years, dec)
                .unwrap()
                .replica_count(1)?;

            // assert
            assert_eq!(total.amount, Decimal::ZERO);
            Ok(())
        }

        #[test]
        // If annual cost=0 => total=0, no matter the decay rate
        fn test_annual_cost_zero() -> Result<()> {
            // setup
            let annual = Amount::new(dec!(0));
            let dec = Amount::new(dec!(0.05)); // 5%
            let years = 10;

            // action
            let total = annual
                .cost_per_replica(years, dec)
                .unwrap()
                .replica_count(1)?;

            // assert
            assert_eq!(total.amount, Decimal::ZERO);
            Ok(())
        }
    }

    mod user_fee {
        use super::*;

        #[test]
        fn test_normal_case() -> Result<()> {
            // Setup
            let cost_per_gb_10_replicas_200_years = dec!(8.65);
            let price_irys = Amount::new(dec!(1.09));
            let bytes_to_store = 1024 * 1024 * 200_u64; // 200mb
            let fee_percentage = dec!(0.05);

            // Action
            let network_fee = Amount {
                amount: cost_per_gb_10_replicas_200_years,
                _t: PhantomData,
            }
            .base_network_fee(bytes_to_store.into(), price_irys)?;
            let price_with_network_reward = network_fee.add_multiplier(fee_percentage)?;

            // Assert
            let expected = Decimal::from_str_exact("1.55")?;
            let diff = (network_fee.amount - expected).abs();
            assert!(diff < Decimal::from_str_exact("0.0001")?);

            // Assert with reward
            let expected = Decimal::from_str_exact("1.63")?;
            let diff = (price_with_network_reward.amount - expected).abs();
            assert!(diff < Decimal::from_str_exact("0.01")?);
            Ok(())
        }
    }

    mod ema_calculations {
        use super::*;

        /// Test a normal scenario where all parameters lead to a valid EMA.
        #[test]
        fn test_calculate_ema_valid() -> Result<()> {
            // setup
            let total_past_blocks = 10;
            let ema_0 = Amount::new(dec!(1.00));
            let current_price = Amount::new(dec!(1.01));

            // action
            let ema_1 = current_price.calculate_ema(total_past_blocks, ema_0)?;

            // assert
            let expected = dec!(1.00181818181818);
            assert!(
                (ema_1.amount - expected).abs() < dec!(0.00000001),
                "EMA is {}, expected around {}",
                ema_1.amount,
                expected
            );
            Ok(())
        }

        /// Test extremely large inputs (`u64::MAX`)
        #[test]
        fn test_calculate_ema_huge_epoch() {
            // setup
            let total_past_blocks = u64::MAX;
            let current_irys_price = Amount::new(dec!(123.456));
            let last_block_ema = Amount::new(dec!(1000.0));

            // action
            let result = current_irys_price.calculate_ema(total_past_blocks, last_block_ema);

            // assert
            result.unwrap();
        }
    }
}
