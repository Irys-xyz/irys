use eyre::Result;
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use std::sync::Mutex;

#[derive(Debug)]
struct PriceContext {
    price: Amount<(IrysPrice, Usd)>,
    // Counts how many times `current_price` has been called
    calls: u64,
    // Tracks whether the price is going up (true) or down (false)
    going_up: bool,
}

/// Mock oracle that will return fluctuating prices for the Irys token
#[derive(Debug)]
pub struct MockOracle {
    context: Mutex<PriceContext>,
    /// Const value change on each call
    incremental_change: Amount<(IrysPrice, Usd)>,
    /// After this many calls, we toggle the direction of change (up/down)
    smoothing_interval: u64,
}

impl MockOracle {
    /// Initialize a new mock oracle
    #[must_use]
    pub const fn new(
        initial_price: Amount<(IrysPrice, Usd)>,
        incremental_change: Amount<(IrysPrice, Usd)>,
        smoothing_interval: u64,
        initial_direction_up: bool,
    ) -> Self {
        let price_context = PriceContext {
            price: initial_price,
            calls: 0,
            going_up: initial_direction_up,
        };
        Self {
            context: Mutex::new(price_context),
            incremental_change,
            smoothing_interval,
        }
    }

    /// Computes the new Irys price and returns it
    ///
    /// # Panics
    ///
    /// If the underlying mutex gets poisoned.
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn current_price(&self) -> Result<Amount<(IrysPrice, Usd)>> {
        let mut guard = self.context.lock().expect("irrecoverable lock poisoned");

        guard.calls = guard.calls.wrapping_add(1);

        // Each time we hit the smoothing interval, toggle the direction
        if guard
            .calls
            .checked_rem(self.smoothing_interval)
            .unwrap_or_default()
            == 0
        {
            guard.going_up = !guard.going_up;
            guard.calls = 0;
            tracing::debug!(new_direction_is_up =? guard.going_up, "inverting the delta direction");
        }

        if guard.going_up {
            guard.price = Amount::new(
                guard
                    .price
                    .amount
                    .saturating_add(self.incremental_change.amount),
            );
        } else {
            guard.price = Amount::new(
                guard
                    .price
                    .amount
                    .saturating_sub(self.incremental_change.amount),
            );
        }

        Ok(Amount::new(guard.price.amount))
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "simpler tests")]
mod tests {
    use super::*;
    use irys_types::storage_pricing::Amount;
    use rust_decimal_macros::dec;

    #[test_log::test(tokio::test)]
    async fn test_initial_price() {
        let smoothing_interval = 2;
        let oracle = MockOracle::new(
            Amount::token(dec!(1.0)).unwrap(),
            Amount::token(dec!(0.05)).unwrap(),
            smoothing_interval,
            true,
        );

        let price = oracle.current_price().expect("Unable to get current price");
        assert_eq!(
            price,
            Amount::token(dec!(1.05)).unwrap(),
            "Initial price should be 1.0"
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_price_increases() {
        let smoothing_interval = 3;
        let oracle = MockOracle::new(
            Amount::token(dec!(1.0)).unwrap(),
            Amount::token(dec!(0.10)).unwrap(),
            smoothing_interval,
            true,
        );

        let _unused_price = oracle.current_price().unwrap();
        let price_after_first = oracle.current_price().unwrap();

        assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.20));
    }

    #[test_log::test(tokio::test)]
    async fn test_toggle_direction() {
        let smoothing_interval = 2;
        let oracle = MockOracle::new(
            Amount::token(dec!(1.0)).unwrap(),
            Amount::token(dec!(0.10)).unwrap(),
            smoothing_interval,
            true,
        );

        let price_after_first = oracle.current_price().unwrap();
        assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.10));

        // Call #2 hits smoothing interval, toggles to down: 1.10 - 0.10 = 1.00
        let price_after_second = oracle.current_price().unwrap();
        assert_eq!(price_after_second.token_to_decimal().unwrap(), dec!(1.00));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use irys_types::U256;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_mock_oracle_price_always_positive(num_updates in 1_u32..200) {
            let initial_price = Amount::new(U256::from(1_000_000_000_000_000_000_u64));
            let increment = Amount::new(U256::from(10_000_000_000_000_000_u64));
            let oracle = MockOracle::new(initial_price, increment, 5, true);

            for _ in 0..num_updates {
                let price = oracle.current_price().expect("should not fail");
                prop_assert!(price.amount > U256::zero(), "price must always be > 0");
            }
        }
    }
}
