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

#[derive(Debug)]
enum OracleSource {
    /// An Oracle that generates the price locally, not suitable for production usage.
    Mock(mock_oracle::MockOracle),
    /// CoinMarketCap-backed oracle
    CoinMarketCap(coinmarketcap::CoinMarketCapOracle),
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
}

impl SingleOracle {
    /// Construct a mock oracle (no background poller).
    pub fn new_mock(
        initial_price: Amount<(IrysPrice, Usd)>,
        incremental_change: Amount<(IrysPrice, Usd)>,
        smoothing_interval: u64,
    ) -> Arc<Self> {
        let source = OracleSource::Mock(mock_oracle::MockOracle::new(
            initial_price,
            incremental_change,
            smoothing_interval,
        ));
        Arc::new(Self {
            source,
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial_price,
                last_updated: std::time::SystemTime::now(),
            })),
        })
    }

    /// Construct a CoinMarketCap oracle and synchronously fetch initial price.
    pub fn new_coinmarketcap_blocking(
        api_key: String,
        symbol: String,
        handle: &tokio::runtime::Handle,
    ) -> Arc<Self> {
        let client = coinmarketcap::CoinMarketCapOracle::new(api_key, symbol);
        let initial = handle
            .block_on(client.current_price())
            .expect("coinmarketcap initial price must succeed");
        Arc::new(Self {
            source: OracleSource::CoinMarketCap(client),
            cache: Arc::new(RwLock::new(PriceCache {
                value: initial,
                last_updated: std::time::SystemTime::now(),
            })),
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

    /// Spawn periodic polling task if this is a timed oracle (CMC). Returns a service handle.
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
}

// External module files
pub mod coinmarketcap;
pub mod mock_oracle;
