//! # Irys Price Oracle Module
//!
//! This module defines the `IrysPriceOracle` enum, which is responsible for
//! fetching the current price of IRYS tokens in USD.

use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};

/// An enum representing all the possible price oracles for IRYS tokens.
#[derive(Debug)]
pub enum IrysPriceOracle {
    /// An Oracle that generates the price locally, not suitable for production usage.
    MockOracle(mock_oracle::MockOracle),
}

impl IrysPriceOracle {
    /// Returns the current price of IRYS in USD.
    ///
    /// # Errors
    ///
    /// If the underlying `current_price()` call fails.
    #[expect(
        clippy::unused_async,
        reason = "will be async once proper oracles get added"
    )]
    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        use IrysPriceOracle::*;
        match self {
            MockOracle(mock_oracle) => mock_oracle.current_price(),
        }
    }
}

/// Self-contained module for the `MockOracle` implementation
pub mod mock_oracle {
    use irys_types::storage_pricing::phantoms::Percentage;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct PriceContext {
        // Shared price
        price: Amount<(IrysPrice, Usd)>,
        // Counts how many times `current_price` has been called
        calls: u64,
        // Tracks whether the price is going up (true) or down (false)
        going_up: bool,
    }

    /// Mock oracle that will return fluctuating prices for the Irys token
    #[derive(Debug)]
    pub struct MockOracle {
        /// mutable price state
        context: Mutex<PriceContext>,
        /// Percent change in decimal form; e.g. dec!(0.05) means 5%
        percent_change: Amount<Percentage>,
        /// After this many calls, we toggle the direction of change (up/down)
        smoothing_interval: u64,
    }

    impl MockOracle {
        /// Initialize a new mock oracle
        #[must_use]
        pub const fn new(
            initial_price: Amount<(IrysPrice, Usd)>,
            percent_change: Amount<Percentage>,
            smoothing_interval: u64,
        ) -> Self {
            let price_context = PriceContext {
                price: initial_price,
                calls: 0,
                going_up: true,
            };
            Self {
                context: Mutex::new(price_context),
                percent_change,
                smoothing_interval,
            }
        }

        /// Computes the new Irys price and returns it
        ///
        /// # Panics
        ///
        /// If the undrlying mutex gets poisoned.
        #[tracing::instrument(skip_all, err)]
        #[expect(
            clippy::unwrap_in_result,
            reason = "lock poisoning is considered irrecoverable in the mock oracle context"
        )]
        pub fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
            let mut guard = self.context.lock().expect("irrecoverable lock poisoned");

            // increment the amount of calls we have made
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

            // Update the price in the current direction
            if guard.going_up {
                // Price goes up by percent_change
                guard.price = guard
                    .price
                    .add_multiplier(self.percent_change)
                    .unwrap_or_else(|_| Amount::token(dec!(1.0)).expect("valid token price"));
            } else {
                // Price goes down by percent_change
                guard.price = guard
                    .price
                    .sub_multiplier(self.percent_change)
                    .unwrap_or_else(|_| Amount::token(dec!(1.0)).expect("valid token price"));
            }

            Ok(Amount::new(guard.price.amount))
        }
    }

    #[cfg(test)]
    #[expect(clippy::unwrap_used, reason = "simplifier tests")]
    mod tests {
        use super::*;
        use irys_types::storage_pricing::Amount;
        use rust_decimal_macros::dec;

        /// Test that the initial price returned by `MockOracle` matches what was configured.
        #[test_log::test(tokio::test)]
        async fn test_initial_price() {
            let smoothing_interval = 2;
            let oracle = MockOracle::new(
                Amount::token(dec!(1.0)).unwrap(),
                Amount::percentage(dec!(0.05)).unwrap(),
                smoothing_interval,
            );

            // Because this is an async method, we must block on the returned Future in a synchronous test.
            let price = oracle.current_price().expect("Unable to get current price");
            assert_eq!(
                price,
                Amount::token(dec!(1.05)).unwrap(),
                "Initial price should be 1.0"
            );
        }

        /// Test that the price increases when `going_up` is true.
        #[test_log::test(tokio::test)]
        async fn test_price_increases() {
            let smoothing_interval = 3;
            let oracle = MockOracle::new(
                Amount::token(dec!(1.0)).unwrap(),
                Amount::percentage(dec!(0.10)).unwrap(),
                smoothing_interval,
            );

            // First call -> should go up by 10%
            let _unused_price = oracle.current_price().unwrap();
            let price_after_first = oracle.current_price().unwrap();

            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.21));
        }

        /// Test that after the smoothing interval is reached, the direction toggles (up to down).
        #[test_log::test(tokio::test)]
        async fn test_toggle_direction() {
            let smoothing_interval = 2;
            let oracle = MockOracle::new(
                Amount::token(dec!(1.0)).unwrap(),
                Amount::percentage(dec!(0.10)).unwrap(),
                smoothing_interval,
            );

            // Call #1 -> going_up = true => 1.0 -> 1.1
            let price_after_first = oracle.current_price().unwrap();
            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.1));

            // Call #2 -> we've now hit the smoothing interval (2),
            //            so it toggles going_up to false before applying the change
            //            => 1.1 -> 1.1 * (1 - 0.10) = 0.99
            let price_after_second = oracle.current_price().unwrap();
            assert_eq!(price_after_second.token_to_decimal().unwrap(), dec!(0.99));
        }
    }
}
