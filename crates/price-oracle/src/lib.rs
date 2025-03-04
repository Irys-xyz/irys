//! # Irys Price Oracle Module
//!
//! This module defines the `IrysPriceOracle` trait, which is responsible for
//! fetching the current price of IRYS tokens in USD.

use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};

/// A trait representing a price oracle for IRYS tokens (in USD).
pub trait IrysPriceOracle {
    /// The error to be returned upon unsuccessful price retrieval
    type Error;

    /// Returns the current price of IRYS in USD.
    ///
    /// The returned value is an [`Amount`] struct carrying a [`Decimal`]
    /// representing the price in USD.
    fn current_price(&self) -> impl Future<Output = Result<Amount<(IrysPrice, Usd)>, Self::Error>>;
}

#[cfg(any(test, feature = "test-utils"))]
pub mod mock_oracle {
    use irys_types::storage_pricing::phantoms::Percentage;
    use std::sync::Mutex;
    use tracing::{Instrument, debug_span};

    use super::*;

    /// Mock oracle that will return fluctuating prices for the Irys token
    #[derive(Debug)]
    pub struct MockOracle {
        // Shared price
        price: Mutex<Amount<(IrysPrice, Usd)>>,
        // Percent change in decimal form; e.g. dec!(0.05) means 5%
        percent_change: Amount<Percentage>,
        // Counts how many times `current_price` has been called
        calls: Mutex<u64>,
        // After this many calls, we toggle the direction of change (up/down)
        smoothing_interval: u64,
        // Tracks whether we're going up (true) or down (false)
        going_up: Mutex<bool>,
    }

    impl MockOracle {
        /// Initialize a new mock oracle
        pub fn new(
            initial_price: Amount<(IrysPrice, Usd)>,
            percent_change: Amount<Percentage>,
            smoothing_interval: u64,
        ) -> Self {
            Self {
                price: Mutex::new(initial_price),
                percent_change,
                calls: Mutex::new(0),
                smoothing_interval,
                going_up: Mutex::new(true),
            }
        }
    }

    impl IrysPriceOracle for MockOracle {
        type Error = eyre::Report;
        fn current_price(
            &self,
        ) -> impl Future<Output = Result<Amount<(IrysPrice, Usd)>, eyre::Report>> {
            async move {
                let mut price = self.price.lock().unwrap();
                let mut calls = self.calls.lock().unwrap();
                let mut going_up = self.going_up.lock().unwrap();

                *calls += 1;

                // Each time we hit the smoothing interval, toggle the direction
                if *calls % self.smoothing_interval == 0 {
                    *going_up = !*going_up;
                    *calls = 0;
                    tracing::debug!(new_direction_is_up =? going_up, "inverting the delta direction");
                }

                // Update the price in the current direction
                if *going_up {
                    // Price goes up by percent_change
                    *price = price
                        .add_multiplier(self.percent_change)
                        .expect("could not add multiplier");
                } else {
                    // Price goes down by percent_change
                    *price = price
                        .sub_multiplier(self.percent_change)
                        .expect("could not deduct multiplier");
                }

                Ok(Amount::new(price.amount))
            }
            .instrument(debug_span!("fetching price"))
        }
    }

    #[cfg(test)]
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
            let price = oracle
                .current_price()
                .await
                .expect("Unable to get current price");
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
            let _ = oracle.current_price();
            let price_after_first = oracle.current_price().await.unwrap();

            // Price should have gone from 1.0 to 1.0 * (1 + 0.10) = 1.10
            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.10));
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
            let price_after_first = oracle.current_price().await.unwrap();
            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.1));

            // Call #2 -> we've now hit the smoothing interval (2),
            //            so it toggles going_up to false before applying the change
            //            => 1.1 -> 1.1 * (1 - 0.10) = 0.99
            let price_after_second = oracle.current_price().await.unwrap();
            assert_eq!(price_after_second.token_to_decimal().unwrap(), dec!(0.99));
        }
    }
}
