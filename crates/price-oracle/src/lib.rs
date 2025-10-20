//! # Irys Price Oracle Module
//!
//! Multi-oracle service. Each configured oracle maintains its own cached value
//! and, when applicable, a background poller to refresh periodically. Consumers
//! read the most recent value across all configured oracles.

use irys_types::TokioServiceHandle;
use irys_types::storage_pricing::{
    Amount,
    phantoms::{IrysPrice, Usd},
};
use std::sync::{Arc, RwLock};
use tokio::time::{Duration, interval};
use tracing::Instrument as _;
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
    last_updated: std::time::SystemTime,
}

/// A single configured oracle (mock or CoinMarketCap) with its own cache.
#[derive(Debug)]
pub struct SingleOracle {
    source: OracleSource,
    cache: Arc<RwLock<PriceCache>>,
    poll_interval: Option<Duration>,
}

impl SingleOracle {
    /// Construct a mock oracle with a configurable background refresh cadence.
    pub fn new_mock(
        initial_price: Amount<(IrysPrice, Usd)>,
        incremental_change: Amount<(IrysPrice, Usd)>,
        smoothing_interval: u64,
        poll_interval_ms: u64,
    ) -> Arc<Self> {
        let source = OracleSource::Mock(mock_oracle::MockOracle::new(
            initial_price,
            incremental_change,
            smoothing_interval,
        ));
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Arc::new(Self {
            source,
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial_price,
                last_updated: std::time::SystemTime::now(),
            })),
            poll_interval: Some(poll_interval),
        })
    }

    /// Construct a CoinMarketCap oracle and fetch initial price.
    pub async fn new_coinmarketcap(
        api_key: String,
        symbol: String,
        poll_interval_ms: u64,
    ) -> eyre::Result<Arc<Self>> {
        let client = coinmarketcap::CoinMarketCapOracle::new(api_key, symbol);
        let initial = client.current_price().await?;
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Ok(Arc::new(Self {
            source: OracleSource::CoinMarketCap(client),
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial,
                last_updated: std::time::SystemTime::now(),
            })),
            poll_interval: Some(poll_interval),
        }))
    }

    /// Construct a CoinGecko oracle and fetch initial price.
    pub async fn new_coingecko(
        api_key: String,
        coin_id: String,
        poll_interval_ms: u64,
    ) -> eyre::Result<Arc<Self>> {
        let client = coingecko::CoinGeckoOracle::new(api_key, coin_id);
        let initial = client.current_price().await?;
        let poll_interval = Duration::from_millis(poll_interval_ms.max(1));
        Ok(Arc::new(Self {
            source: OracleSource::CoinGecko(client),
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial,
                last_updated: std::time::SystemTime::now(),
            })),
            poll_interval: Some(poll_interval),
        }))
    }

    /// Returns the last cached price of IRYS in USD.
    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        let guard = self
            .cache
            .read()
            .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
        Ok(guard.value)
    }

    /// Spawn periodic polling task when the oracle has a configured update cadence. Returns a service handle.
    pub fn spawn_poller(
        this: Arc<Self>,
        runtime_handle: &tokio::runtime::Handle,
    ) -> Option<TokioServiceHandle> {
        let poll_interval = this.poll_interval?;

        match &this.source {
            OracleSource::Mock(_) | OracleSource::CoinMarketCap(_) | OracleSource::CoinGecko(_) => {
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
            OracleSource::CoinGecko(cg) => cg.current_price().await,
        }?;
        let mut guard = self
            .cache
            .write()
            .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
        guard.value = price;
        guard.last_updated = std::time::SystemTime::now();
        Ok(())
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

    pub async fn current_price(&self) -> eyre::Result<Amount<(IrysPrice, Usd)>> {
        let mut best_ts: Option<std::time::SystemTime> = None;
        let mut best_val: Option<Amount<(IrysPrice, Usd)>> = None;
        for o in &self.oracles {
            let guard = o
                .cache
                .read()
                .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
            if best_ts.map(|t| guard.last_updated > t).unwrap_or(true) {
                best_ts = Some(guard.last_updated);
                best_val = Some(guard.value);
            }
        }
        best_val.ok_or_else(|| eyre::eyre!("no oracles configured"))
    }

    /// Returns the freshest price along with its last_updated timestamp.
    pub async fn current_snapshot(
        &self,
    ) -> eyre::Result<(Amount<(IrysPrice, Usd)>, std::time::SystemTime)> {
        let mut best_ts: Option<std::time::SystemTime> = None;
        let mut best_val: Option<Amount<(IrysPrice, Usd)>> = None;
        for o in &self.oracles {
            let guard = o
                .cache
                .read()
                .map_err(|_| eyre::eyre!("oracle price cache lock poisoned"))?;
            if best_ts.map(|t| guard.last_updated > t).unwrap_or(true) {
                best_ts = Some(guard.last_updated);
                best_val = Some(guard.value);
            }
        }
        match (best_val, best_ts) {
            (Some(v), Some(ts)) => Ok((v, ts)),
            _ => eyre::bail!("no oracles configured"),
        }
    }
}
