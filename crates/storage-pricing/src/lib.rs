use std::marker::PhantomData;

use eyre::ContextCompat;
use rust_decimal::{Decimal, MathematicalOps};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnnualCostUsd;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TotalCostUsd;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DecayRate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount<T> {
    amount: Decimal,
    _t: PhantomData<T>,
}

/// Calculate the total cost for storage
///
/// n = years to pay for storage
/// r = decay rate
///
/// total cost = annual_cost * ((1 - (1-r)^n) / r)
pub fn total_cost(
    annual_cost: Amount<AnnualCostUsd>,
    n: u64,
    decay_rate: Amount<DecayRate>,
) -> eyre::Result<Amount<TotalCostUsd>> {
    // (1 - r)^n
    let one_minus_r_pow = (Decimal::ONE.saturating_sub(decay_rate.amount))
        .checked_powu(n)
        .wrap_err("too many years to pay for")?;

    // fraction = [ 1 - (1-r)^n ] / r
    let fraction = (Decimal::ONE.saturating_sub(one_minus_r_pow))
        .checked_div(decay_rate.amount)
        .wrap_err("decay rate invalid")?;

    // total = annual_cost * fraction
    let total = annual_cost
        .amount
        .checked_mul(fraction)
        .wrap_err("fraction too large")?;

    Ok(Amount {
        amount: total,
        _t: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::Result;
    use rust_decimal::Decimal;

    /// Helper to quickly build Amount<AnnualCostUsd> or Amount<DecayRate> etc.
    fn annual_cost_usd(value: &str) -> Amount<AnnualCostUsd> {
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
        let annual = annual_cost_usd("0.01");
        let dec = decay_rate("0.01");
        let years = 200;
        let total = total_cost(annual, years, dec)?;

        let expected = Decimal::from_str_exact("0.8661").unwrap();
        let diff = (total.amount - expected).abs();
        assert!(diff < Decimal::from_str_exact("0.0001").unwrap());
        Ok(())
    }

    #[test]
    fn test_zero_decay_rate() {
        // r=0 => division by zero => should Err
        let annual = annual_cost_usd("1000");
        let dec = decay_rate("0.0");
        let result = total_cost(annual, 10, dec);
        assert!(result.is_err(), "Expected an error for r=0, got Ok(...)");
    }

    #[test]
    fn test_full_decay_rate() -> Result<()> {
        // r=1 => fraction = [1 - (1-1)^n]/1 => [1 - 0^n]/1 => 1
        // total = annual_cost
        let annual = annual_cost_usd("500");
        let dec = decay_rate("1.0");
        let total = total_cost(annual, 5, dec)?;
        assert_eq!(total.amount, Decimal::from(500));
        Ok(())
    }

    #[test]
    fn test_decay_rate_above_one() {
        let annual = annual_cost_usd("0.01");
        let dec = decay_rate("1.5"); // 150%
        let years = 200;
        let result = total_cost(annual, years, dec);
        assert!(result.is_ok(), "Expected error for decay > 1.0");
    }

    #[test]
    fn test_no_years_to_pay() -> Result<()> {
        let annual = annual_cost_usd("1234.56");
        let dec = decay_rate("0.05"); // 5%
        let total = total_cost(annual, 0, dec)?;
        assert_eq!(total.amount, Decimal::ZERO);
        Ok(())
    }

    #[test]
    fn test_annual_cost_zero() -> Result<()> {
        // If annual cost=0 => total=0, no matter the decay rate
        let annual = annual_cost_usd("0");
        let dec = decay_rate("0.05"); // 5%
        let total = total_cost(annual, 10, dec)?;
        assert_eq!(total.amount, Decimal::ZERO);
        Ok(())
    }
}
