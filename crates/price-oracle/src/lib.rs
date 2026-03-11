//! # Irys Price Oracle Module
//!
//! Multi-oracle service. Each configured oracle maintains its own cached value
//! and, when applicable, a background poller to refresh periodically. Consumers
//! read the most recent value across all configured oracles.

use irys_types::TokioServiceHandle;
use irys_types::UnixTimestamp;
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use std::sync::{Arc, RwLock};
use tokio::time::{Duration, interval};
use tracing::Instrument as _;

#[derive(Debug, thiserror::Error)]
pub enum PriceOracleError {
    #[error("no oracles configured")]
    NoOraclesConfigured,
}

pub mod coingecko;
pub mod coinmarketcap;
pub mod mock_oracle;

#[derive(Debug)]
enum OracleSource {
    /// An Oracle that generates the price locally, not suitable for production usage.
    Mock(mock_oracle::MockOracle),
    /// CoinMarketCap-backed oracle
    CoinMarketCap(coinmarketcap::CoinMarketCapOracle),
    /// CoinGecko-backed oracle
    CoinGecko(coingecko::CoinGeckoOracle),
}

#[derive(Debug, Clone, Copy)]
struct PriceCache {
    value: Amount<(IrysPrice, Usd)>,
    last_updated: UnixTimestamp,
}

/// A single configured oracle (mock or CoinMarketCap) with its own cache.
#[derive(Debug)]
pub struct SingleOracle {
    source: OracleSource,
    cache: Arc<RwLock<PriceCache>>,
    poll_interval: Duration,
}

impl SingleOracle {
    /// Construct a mock oracle with a configurable background refresh cadence.
    pub fn new_mock(
        initial_price: Amount<(IrysPrice, Usd)>,
        incremental_change: Amount<(IrysPrice, Usd)>,
        smoothing_interval: u64,
        initial_direction_up: bool,
        poll_interval_ms: u64,
    ) -> Arc<Self> {
        let source = OracleSource::Mock(mock_oracle::MockOracle::new(
            initial_price,
            incremental_change,
            smoothing_interval,
            initial_direction_up,
        ));
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Arc::new(Self {
            source,
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial_price,
                last_updated: UnixTimestamp::now().unwrap_or_default(),
            })),
            poll_interval,
        })
    }

    /// Construct a CoinMarketCap oracle and fetch initial price.
    pub async fn new_coinmarketcap(
        api_key: String,
        id: String,
        poll_interval_ms: u64,
    ) -> eyre::Result<Arc<Self>> {
        let client = coinmarketcap::CoinMarketCapOracle::new(api_key, id);
        let coinmarketcap::CoinMarketCapQuote {
            amount: initial_amount,
            last_updated: initial_last_updated,
        } = client.current_price().await?;
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Ok(Arc::new(Self {
            source: OracleSource::CoinMarketCap(client),
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial_amount,
                last_updated: initial_last_updated,
            })),
            poll_interval,
        }))
    }

    /// Construct a CoinGecko oracle and fetch initial price.
    pub async fn new_coingecko(
        api_key: String,
        coin_id: String,
        demo_api_key: bool,
        poll_interval_ms: u64,
    ) -> eyre::Result<Arc<Self>> {
        let client = coingecko::CoinGeckoOracle::new(api_key, coin_id, demo_api_key);
        let coingecko::CoinGeckoQuote {
            amount: initial_amount,
            last_updated: initial_last_updated,
        } = client.current_price().await?;
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Ok(Arc::new(Self {
            source: OracleSource::CoinGecko(client),
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial_amount,
                last_updated: initial_last_updated,
            })),
            poll_interval,
        }))
    }

    /// Returns the last cached price of IRYS in USD.
    pub fn current_price(&self) -> Amount<(IrysPrice, Usd)> {
        let guard = self.cache.read().expect("oracle price cache lock poisoned");
        guard.value
    }

    /// Spawn periodic polling task when the oracle has a configured update cadence. Returns a service handle.
    pub fn spawn_poller(
        self: Arc<Self>,
        runtime_handle: &tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let poll_interval = self.poll_interval;

        let (shutdown_tx, mut shutdown_rx) = reth::tasks::shutdown::signal();
        let handle = runtime_handle.spawn(
            async move {
                let mut ticker = interval(poll_interval);
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            tracing::info!("price oracle poller shutdown");
                            break;
                        }
                        _ = ticker.tick() => {
                            if let Err(err) = self.update_once().await {
                                tracing::error!(?err, "oracle price fetch failed");
                            }
                        }
                    }
                }
            }
            .in_current_span(),
        );
        TokioServiceHandle {
            name: "price_oracle_poller".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn update_once(&self) -> eyre::Result<()> {
        match &self.source {
            OracleSource::Mock(m) => {
                let amount = m.current_price()?;
                self.update_cache(amount, UnixTimestamp::now().unwrap_or_default())
            }
            OracleSource::CoinMarketCap(c) => {
                let coinmarketcap::CoinMarketCapQuote {
                    amount,
                    last_updated,
                } = c.current_price().await?;
                self.update_cache(amount, last_updated)
            }
            OracleSource::CoinGecko(cg) => {
                let coingecko::CoinGeckoQuote {
                    amount,
                    last_updated,
                } = cg.current_price().await?;
                self.update_cache(amount, last_updated)
            }
        }
        Ok(())
    }

    fn update_cache(&self, amount: Amount<(IrysPrice, Usd)>, timestamp: UnixTimestamp) {
        let mut guard = self
            .cache
            .write()
            .expect("oracle price cache lock poisoned");
        guard.value = amount;
        guard.last_updated = timestamp;
    }
}

/// Aggregates multiple oracles and returns the freshest (latest-updated) price.
#[derive(Debug)]
pub struct IrysPriceOracle {
    oracles: Vec<Arc<SingleOracle>>,
}

impl IrysPriceOracle {
    pub fn new(oracles: Vec<Arc<SingleOracle>>) -> Arc<Self> {
        Arc::new(Self { oracles })
    }

    /// Returns the freshest price along with its last_updated timestamp (in seconds).
    ///
    /// Returns `Err` when no oracles are configured — callers should fall back
    /// to the parent block's oracle price in that case.
    pub fn current_snapshot(
        &self,
    ) -> Result<(Amount<(IrysPrice, Usd)>, UnixTimestamp), PriceOracleError> {
        let mut best_ts: Option<UnixTimestamp> = None;
        let mut best_val: Option<Amount<(IrysPrice, Usd)>> = None;
        for o in &self.oracles {
            let guard = o.cache.read().expect("oracle price cache lock poisoned");
            if best_ts.map(|t| guard.last_updated > t).unwrap_or(true) {
                best_ts = Some(guard.last_updated);
                best_val = Some(guard.value);
            }
        }
        match (best_val, best_ts) {
            (Some(v), Some(ts)) => Ok((v, ts)),
            _ => Err(PriceOracleError::NoOraclesConfigured),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "simpler tests")]
mod tests {
    use super::*;
    use irys_types::U256;
    use rstest::rstest;

    fn make_oracle_with_cache(price_raw: u64, timestamp_secs: u64) -> Arc<SingleOracle> {
        let price = Amount::new(U256::from(price_raw));
        let increment = Amount::new(U256::from(1_u64));
        let oracle = SingleOracle::new_mock(price, increment, 10, true, 1000);
        {
            let mut guard = oracle.cache.write().unwrap();
            guard.value = price;
            guard.last_updated = UnixTimestamp::from_secs(timestamp_secs);
        }
        oracle
    }

    #[rstest]
    #[case::third_is_freshest(100, 200, 300, 2)]
    #[case::first_is_freshest(300, 200, 100, 0)]
    #[case::middle_is_freshest(100, 300, 200, 1)]
    #[case::equal_timestamps_first_wins(300, 300, 100, 0)]
    fn test_current_snapshot_returns_freshest(
        #[case] ts_a: u64,
        #[case] ts_b: u64,
        #[case] ts_c: u64,
        #[case] expected_idx: usize,
    ) {
        let prices = [1_000_000_u64, 2_000_000_u64, 3_000_000_u64];
        let timestamps = [ts_a, ts_b, ts_c];
        let oracles: Vec<Arc<SingleOracle>> = prices
            .iter()
            .zip(timestamps.iter())
            .map(|(&p, &t)| make_oracle_with_cache(p, t))
            .collect();

        let aggregator = IrysPriceOracle::new(oracles);
        let (val, ts) = aggregator.current_snapshot().unwrap();

        assert_eq!(ts, UnixTimestamp::from_secs(timestamps[expected_idx]));
        assert_eq!(val.amount, U256::from(prices[expected_idx]));
    }

    #[test]
    fn test_current_snapshot_no_oracles_returns_error() {
        let aggregator = IrysPriceOracle { oracles: vec![] };
        assert!(aggregator.current_snapshot().is_err());
    }
}
