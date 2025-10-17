//! # Irys Price Oracle Module
//!
//! Background service that periodically fetches the IRYS price and caches it
//! for fast reads without issuing network requests on demand.

use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use irys_types::TokioServiceHandle;
use std::sync::{Arc, RwLock};
use tokio::time::{interval, Duration};
use tracing::Instrument as _;

#[derive(Debug)]
enum OracleSource {
    /// An Oracle that generates the price locally, not suitable for production usage.
    Mock(mock_oracle::MockOracle),
    /// CoinMarketCap-backed oracle
    CoinMarketCap(coinmarketcap::CoinMarketCapOracle),
}

#[derive(Debug, Default)]
struct PriceCache {
    value: Amount<(IrysPrice, Usd)>,
}

/// Public oracle handle. Spawns a background poller to keep `cache` up-to-date.
#[derive(Debug)]
pub struct IrysPriceOracle {
    source: OracleSource,
    cache: Arc<RwLock<PriceCache>>,
}

impl IrysPriceOracle {
    /// Construct a mock oracle and spawn the hourly poller.
    pub fn new_mock(
        initial_price: Amount<(IrysPrice, Usd)>,
        incremental_change: Amount<(IrysPrice, Usd)>,
        smoothing_interval: u64,
    ) -> Arc<Self> {
        // Initialize underlying oracle and fetch initial value immediately
        let source = OracleSource::Mock(mock_oracle::MockOracle::new(
            initial_price,
            incremental_change,
            smoothing_interval,
        ));
        // Use the configured initial price for the starting cache value
        let initial = initial_price;

        Arc::new(Self { source, cache: Arc::new(RwLock::new(PriceCache { value: initial })) })
    }

    /// Construct a CoinMarketCap oracle and spawn the hourly poller.
    pub fn new_coinmarketcap_blocking(
        api_key: String,
        symbol: String,
        handle: &tokio::runtime::Handle,
    ) -> Arc<Self> {
        let client = coinmarketcap::CoinMarketCapOracle::new(api_key, symbol);
        let initial = handle
            .block_on(client.current_price())
            .expect("coinmarketcap initial price fetch must succeed");
        Arc::new(Self {
            source: OracleSource::CoinMarketCap(client),
            cache: Arc::new(RwLock::new(PriceCache { value: initial })),
        })
    }

    /// Returns the last cached price of IRYS in USD.
    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        let guard = self
            .cache
            .read()
            .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
        Ok(guard.value)
    }

    pub fn spawn_poller(
        this: Arc<Self>,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Option<TokioServiceHandle> {
        match &this.source {
            OracleSource::Mock(_) => None,
            OracleSource::CoinMarketCap(_) => {
                let (shutdown_tx, mut shutdown_rx) = reth::tasks::shutdown::signal();
                let handle = runtime_handle.spawn(
                    async move {
                        let mut ticker = interval(Duration::from_secs(60 * 60));
                        loop {
                            tokio::select! {
                                _ = &mut shutdown_rx => {
                                    tracing::info!("price oracle poller shutdown");
                                    break;
                                }
                                _ = ticker.tick() => {
                                    if let Err(err) = this.update_once().await {
                                        tracing::warn!(?err, "oracle price fetch failed");
                                    }
                                }
                            }
                        }
                    }
                    .in_current_span(),
                );
                Some(TokioServiceHandle {
                    name: "price_oracle_poller".to_string(),
                    handle,
                    shutdown_signal: shutdown_tx,
                })
            }
        }
    }

    #[tracing::instrument(skip(self), err)]
    async fn update_once(&self) -> eyre::Result<()> {
        let price = match &self.source {
            OracleSource::Mock(m) => m.current_price(),
            OracleSource::CoinMarketCap(c) => c.current_price().await,
        }?;
        let mut guard = self
            .cache
            .write()
            .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
        guard.value = price;
        Ok(())
    }
}

/// Self-contained module for the `MockOracle` implementation
pub mod mock_oracle {

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
        /// Mutable price state
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
        ) -> Self {
            let price_context = PriceContext {
                price: initial_price,
                calls: 0,
                going_up: true,
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
                // Price goes up
                guard.price = Amount::new(
                    guard
                        .price
                        .amount
                        .saturating_add(self.incremental_change.amount),
                );
            } else {
                // Price goes down
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

        /// Test that the initial price returned by `MockOracle` matches what was configured.
        #[test_log::test(tokio::test)]
        async fn test_initial_price() {
            let smoothing_interval = 2;
            let oracle = MockOracle::new(
                Amount::token(dec!(1.0)).unwrap(),
                Amount::token(dec!(0.05)).unwrap(),
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
                Amount::token(dec!(0.10)).unwrap(),
                smoothing_interval,
            );

            // First call -> should go up by 0.10 to 1.10
            let _unused_price = oracle.current_price().unwrap();
            // Second call -> should go up by another 0.10 to 1.20
            let price_after_first = oracle.current_price().unwrap();

            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.20));
        }

        /// Test that after the smoothing interval is reached, the direction toggles (up to down).
        #[test_log::test(tokio::test)]
        async fn test_toggle_direction() {
            let smoothing_interval = 2;
            let oracle = MockOracle::new(
                Amount::token(dec!(1.0)).unwrap(),
                Amount::token(dec!(0.10)).unwrap(),
                smoothing_interval,
            );

            // Call #1 -> going_up = true => 1.0 + 0.10 = 1.10
            let price_after_first = oracle.current_price().unwrap();
            assert_eq!(price_after_first.token_to_decimal().unwrap(), dec!(1.10));

            // Call #2 -> we've now hit the smoothing interval (2),
            //            so it toggles going_up to false before applying the change
            //            => 1.10 - 0.10 = 1.00
            let price_after_second = oracle.current_price().unwrap();
            assert_eq!(price_after_second.token_to_decimal().unwrap(), dec!(1.00));
        }
    }
}

/// CoinMarketCap oracle implementation
pub mod coinmarketcap {
    use super::*;
    use eyre::{bail, eyre, Context};
    use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
    use reqwest::Client;
    use rust_decimal::Decimal;
    use serde_json::Value;
    use std::str::FromStr;

    #[derive(Debug, Clone)]
    pub struct CoinMarketCapOracle {
        client: Client,
        api_key: String,
        symbol: String,
    }

    impl CoinMarketCapOracle {
        /// Create a new CoinMarketCap oracle client
        #[must_use]
        pub fn new(
            api_key: String,
            symbol: String,
        ) -> Self {
            let client = Client::new();
            Self { client, api_key, symbol }
        }

        #[tracing::instrument(skip_all, err)]
        pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
            // Build headers
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
            headers.insert(
                "X-CMC_PRO_API_KEY",
                HeaderValue::from_str(&self.api_key).context("invalid API key header value")?,
            );

            // Build query params
            let mut params: Vec<(String, String)> = Vec::new();
            params.push(("symbol".to_string(), self.symbol.clone()));
            params.push(("convert".to_string(), "USD".to_string()));

            let resp = self
                .client
                .get("https://pro-api.coinmarketcap.com/v2/cryptocurrency/quotes/latest")
                .headers(headers)
                .query(&params)
                .send()
                .await
                .context("failed to call CoinMarketCap API")?;

            if !resp.status().is_success() {
                bail!("CoinMarketCap API returned HTTP status {}", resp.status());
            }

            let body: Value = resp.json().await.context("failed to parse CMC JSON body")?;

            // check status
            if let Some(status_obj) = body.get("status").and_then(|v| v.as_object()) {
                let error_code = status_obj.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0);
                if error_code != 0 {
                    let msg = status_obj
                        .get("error_message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    bail!("CoinMarketCap API error {}: {}", error_code, msg);
                }
            }

            let price_dec = extract_price_from_cmc(&body, "USD")
                .context("failed to extract price from CMC response")?;

            let amount = Amount::<(IrysPrice, Usd)>::token(price_dec)
                .context("failed to convert price to Amount")?;
            Ok(amount)
        }
    }

    #[tracing::instrument(skip_all, err)]
    fn extract_price_from_cmc(v: &Value, convert: &str) -> eyre::Result<Decimal> {
        let data = v
            .get("data")
            .and_then(|d| d.as_object())
            .ok_or_else(|| eyre!("missing or invalid 'data' field"))?;

        // Normalize convert key (expect uppercase like "USD")
        let convert_key = if convert.is_empty() { "USD" } else { convert };

        for (_k, entry) in data.iter() {
            // Some responses wrap entry in an array when using symbol
            let items: Vec<&Value> = if let Some(arr) = entry.as_array() {
                arr.iter().collect()
            } else {
                vec![entry]
            };

            for item in items {
                if let Some(quote_obj) = item.get("quote").and_then(|q| q.as_object()) {
                    if let Some(currency_obj) = quote_obj.get(convert_key).and_then(|c| c.as_object()) {
                        if let Some(price_val) = currency_obj.get("price") {
                            // Convert price to Decimal robustly
                            if let Some(n) = price_val.as_f64() {
                                if let Some(d) = Decimal::from_f64_retain(n) {
                                    return Ok(d);
                                }
                            }
                            // Fallback: parse from string representation
                            let s = price_val.to_string();
                            if let Ok(d) = Decimal::from_str(&s) {
                                return Ok(d);
                            }
                        }
                    }
                }
            }
        }

        Err(eyre!("could not find price in CMC response"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rust_decimal_macros::dec;

        #[test]
        fn parse_sample_response() {
            let sample = r#"{
"data": {
"1": {
"id": 1,
"name": "Bitcoin",
"symbol": "BTC",
"slug": "bitcoin",
"is_active": 1,
"is_fiat": 0,
"circulating_supply": 17199862,
"total_supply": 17199862,
"max_supply": 21000000,
"date_added": "2013-04-28T00:00:00.000Z",
"num_market_pairs": 331,
"cmc_rank": 1,
"last_updated": "2018-08-09T21:56:28.000Z",
"tags": [
"mineable"
],
"platform": null,
"self_reported_circulating_supply": null,
"self_reported_market_cap": null,
"quote": {
"USD": {
"price": 6602.60701122,
"volume_24h": 4314444687.5194,
"volume_change_24h": -0.152774,
"percent_change_1h": 0.988615,
"percent_change_24h": 4.37185,
"percent_change_7d": -12.1352,
"percent_change_30d": -12.1352,
"market_cap": 852164659250.2758,
"market_cap_dominance": 51,
"fully_diluted_market_cap": 952835089431.14,
"last_updated": "2018-08-09T21:56:28.000Z"
}
}
}
},
"status": {
"timestamp": "2025-10-13T00:00:34.078Z",
"error_code": 0,
"error_message": "",
"elapsed": 10,
"credit_count": 1,
"notice": ""
}
}"#;

            let v: Value = serde_json::from_str(sample).unwrap();
            let d = extract_price_from_cmc(&v, "USD").unwrap();
            assert_eq!(d, dec!(6602.60701122));
        }
    }
}
