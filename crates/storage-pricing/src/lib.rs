use std::{fmt::Debug, marker::PhantomData};

use eyre::ContextCompat;
use rust_decimal::{Decimal, MathematicalOps};
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnnualCostUsdPerGb;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnnualCostUsdPerByte;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DecayRate;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetworkFee;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnnualUsdCostPerReplicaPerByte;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount<T> {
    amount: Decimal,
    _t: PhantomData<T>,
}

impl Amount<AnnualCostUsdPerGb> {
    pub fn cost_per_byte(self) -> Amount<AnnualCostUsdPerByte> {
        let amount = self
            .amount
            .checked_div(dec!(1024.0))
            .expect("cost per byte must always be derrivable");

        Amount {
            amount,
            _t: PhantomData,
        }
    }
}

impl Amount<AnnualCostUsdPerByte> {
    /// Calculate the total cost for storage.
    /// The price is for storing a single replica.
    ///
    /// n = years to pay for storage
    /// r = decay rate
    ///
    /// total cost = annual_cost * ((1 - (1-r)^n) / r)
    pub fn cost_per_replica(
        self,
        years_to_pay_for_storage: u64,
        decay_rate: Amount<DecayRate>,
    ) -> eyre::Result<Amount<AnnualUsdCostPerReplicaPerByte>> {
        let annual_cost_per_byte = self.amount;

        // (1 - r)^n
        let one_minus_r_pow = (Decimal::ONE.saturating_sub(decay_rate.amount))
            .checked_powu(years_to_pay_for_storage)
            .wrap_err("too many years to pay for")?;

        // fraction = [ 1 - (1-r)^n ] / r
        let fraction = (Decimal::ONE.saturating_sub(one_minus_r_pow))
            .checked_div(decay_rate.amount)
            .wrap_err("decay rate invalid")?;

        // total = annual_cost * fraction
        let total = annual_cost_per_byte
            .checked_mul(fraction)
            .wrap_err("fraction too large")?;

        Ok(Amount {
            amount: total,
            _t: PhantomData,
        })
    }
}

impl Amount<AnnualUsdCostPerReplicaPerByte> {
    pub fn replica_count(self, replicas: u64) -> eyre::Result<Self> {
        let amount = self
            .amount
            .checked_mul(replicas.into())
            .wrap_err("overflow during replica multiplication")?;
        Ok(Self { amount, ..self })
    }
    pub fn base_network_fee(
        self,
        bytes_to_store: u128,
        irys_token_price: Decimal,
    ) -> eyre::Result<Amount<NetworkFee>> {
        // annual cost per replica per byte in usd
        let usd_fee = self.amount.checked_mul(bytes_to_store.into()).unwrap();
        // converted to $IRYS
        let network_fee = usd_fee.checked_div(irys_token_price).unwrap();
        Ok(Amount {
            amount: network_fee,
            _t: PhantomData,
        })
    }
}

impl Amount<NetworkFee> {
    /// Add additional network fee for storing data to incerace incentivisation.
    ///
    /// 0.05 => 5%
    /// 1.00 => 100%
    /// 0.50 => 50%
    pub fn add_multiplier(self, percentage: Decimal) -> Amount<NetworkFee> {
        Self {
            amount: self.amount.saturating_mul(percentage),
            _t: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::Result;
    use rust_decimal::Decimal;

    /// Helper to quickly build Amount<AnnualCostUsd> or Amount<DecayRate> etc.
    fn annual_cost_usd(value: &str) -> Amount<AnnualCostUsdPerGb> {
        Amount {
            amount: Decimal::from_str_exact(value).unwrap(),
            _t: PhantomData,
        }
    }
    fn decay_rate(value: &str) -> Amount<DecayRate> {
        Amount {
            amount: Decimal::from_str_exact(value).unwrap(),
            _t: PhantomData,
        }
    }

    #[test]
    fn test_normal_case() -> Result<()> {
        // Setup
        let annual = annual_cost_usd("0.01");
        let dec = decay_rate("0.01");
        let years = 200;

        // Action
        let cost_per_replica = annual
            .cost_per_byte()
            .cost_per_replica(years, dec)?
            .replica_count(1)?;
        let cost_per_gb = cost_per_replica.amount * dec!(1024.0);

        // Assert - cost per gb / single replica
        let expected = Decimal::from_str_exact("0.8661")?;
        let diff = (cost_per_gb - expected).abs();
        assert!(diff < Decimal::from_str_exact("0.0001")?);

        // Assert - cost per gb / 10 replicas
        let cost_per_10_replicas = cost_per_replica.replica_count(10)?;
        let cost_per_10_in_gb = cost_per_10_replicas.amount * dec!(1024.0);
        let expected = Decimal::from_str_exact("8.66")?;
        let diff = (cost_per_10_in_gb - expected).abs();
        assert!(diff < Decimal::from_str_exact("0.001")?);
        Ok(())
    }

    #[test]
    // r=0 => division by zero => should Err
    fn test_zero_decay_rate() {
        // setup
        let annual = annual_cost_usd("1000");
        let dec = decay_rate("0.0");
        let years = 10;

        // actoin
        let result = annual.cost_per_byte().cost_per_replica(years, dec);

        // assert
        assert!(result.is_err(), "Expected an error for r=0, got Ok(...)");
    }

    #[test]
    // r=1 => fraction = (1 - (1-1)^n)/1 => (1 - 0^n)/1 => 1
    fn test_full_decay_rate() -> Result<()> {
        // setup
        let annual = annual_cost_usd("500");
        let dec = decay_rate("1.0");
        let years_to_pay_for_storage = 5;

        // action
        let total = annual
            .cost_per_byte()
            .cost_per_replica(years_to_pay_for_storage, dec)
            .unwrap()
            .replica_count(1)?;
        let per_gb = total.amount * dec!(1024.0);

        // assert
        assert_eq!(per_gb, Decimal::from(500));
        Ok(())
    }

    #[test]
    fn test_decay_rate_above_one() {
        // setup
        let annual = annual_cost_usd("0.01");
        let dec = decay_rate("1.5"); // 150%
        let years = 200;

        // actoin
        let result = annual
            .cost_per_byte()
            .cost_per_replica(years, dec)
            .unwrap()
            .replica_count(1);

        // assert
        assert!(result.is_ok(), "Expected error for decay > 1.0");
    }

    #[test]
    fn test_no_years_to_pay() -> Result<()> {
        // setup
        let annual = annual_cost_usd("1234.56");
        let dec = decay_rate("0.05"); // 5%
        let years = 0;

        // action
        let total = annual
            .cost_per_byte()
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
        let annual = annual_cost_usd("0");
        let dec = decay_rate("0.05"); // 5%
        let years = 10;

        // action
        let total = annual
            .cost_per_byte()
            .cost_per_replica(years, dec)
            .unwrap()
            .replica_count(1)?;

        // assert
        assert_eq!(total.amount, Decimal::ZERO);
        Ok(())
    }
}
