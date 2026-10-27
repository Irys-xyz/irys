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

/// Maximum seconds an oracle-reported `last_updated_at` may lead the local wall clock.
/// A price that is further in the future than this bound is rejected. The startup fetch
/// then fails, so the node does not start when the local clock or the oracle clock is
/// skewed by more than this bound. A larger bound accepts more skew but also accepts
/// prices that are stamped further ahead of real time.
pub const MAX_ORACLE_CLOCK_SKEW_SECS: u64 = 60;

/// Fails when `reported` is more than `MAX_ORACLE_CLOCK_SKEW_SECS` ahead of the local wall clock.
/// A clock set before the UNIX epoch is treated as time zero.
pub(crate) fn reject_future_timestamp(reported: UnixTimestamp) -> eyre::Result<()> {
    let now_secs = UnixTimestamp::now().map_or(0, UnixTimestamp::as_secs);
    reject_future_timestamp_at(reported, now_secs)
}

fn reject_future_timestamp_at(reported: UnixTimestamp, now_secs: u64) -> eyre::Result<()> {
    let latest_accepted = now_secs.saturating_add(MAX_ORACLE_CLOCK_SKEW_SECS);
    if reported.as_secs() > latest_accepted {
        eyre::bail!(
            "oracle timestamp {} is {}s ahead of the local clock (max skew {}s); check the system clock",
            reported.as_secs(),
            reported.as_secs() - now_secs,
            MAX_ORACLE_CLOCK_SKEW_SECS
        );
    }
    Ok(())
}

/// Fails when the feed reports a zero price. Block validation rejects a zero oracle price.
/// The producer bounds the price to `token_price_safe_range` around the parent price. Only
/// when that range is 100% or more does the lower bound reach zero. A zero price then passes
/// the bound and the produced block is invalid. With a smaller range, the bound raises a zero
/// price to a non-zero minimum.
pub(crate) fn reject_zero_price(amount: Amount<(IrysPrice, Usd)>) -> eyre::Result<()> {
    eyre::ensure!(!amount.amount.is_zero(), "oracle reported a zero price");
    Ok(())
}

/// Total time allowed for one oracle HTTP request, so a hung endpoint cannot wedge the poller.
pub const ORACLE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest oracle response body accepted; real quote responses are a few hundred bytes.
pub const MAX_ORACLE_RESPONSE_BYTES: u64 = 1024 * 1024;

pub(crate) fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(ORACLE_HTTP_TIMEOUT)
        .read_timeout(ORACLE_HTTP_TIMEOUT)
        .build()
        .expect("reqwest client builds with a fixed timeout")
}

/// Reads the body, failing once it exceeds `MAX_ORACLE_RESPONSE_BYTES`.
pub(crate) async fn read_capped_body(mut response: reqwest::Response) -> eyre::Result<Vec<u8>> {
    let cap = MAX_ORACLE_RESPONSE_BYTES;
    if let Some(len) = response.content_length() {
        eyre::ensure!(
            len <= cap,
            "oracle response Content-Length {len} exceeds cap of {cap} bytes"
        );
    }
    let mut buf = Vec::new();
    let mut total = 0_u64;
    while let Some(chunk) = response.chunk().await? {
        let chunk_len = u64::try_from(chunk.len())?;
        total = irys_types::accumulate_response_bytes(total, chunk_len, cap)
            .map_err(|_| eyre::eyre!("oracle response exceeds cap of {cap} bytes"))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Attempts for the startup price fetch before the node reports an error.
pub const INITIAL_FETCH_ATTEMPTS: u32 = 5;

/// Delay before the second startup fetch attempt; doubles after each further failure.
pub const INITIAL_FETCH_BASE_BACKOFF: Duration = Duration::from_secs(1);

/// Runs `fetch` up to `attempts` times, doubling the delay between failures.
/// Returns the last error, with the attempt count, once all attempts fail.
async fn fetch_with_backoff<T, F, Fut>(
    what: &str,
    attempts: u32,
    base_backoff: Duration,
    mut fetch: F,
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<T>>,
{
    let mut backoff = base_backoff;
    let mut attempt = 1_u32;
    loop {
        match fetch().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt >= attempts => {
                return Err(err.wrap_err(format!(
                    "{what}: initial price fetch failed after {attempts} attempts"
                )));
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    attempt,
                    ?backoff,
                    "{what}: initial price fetch failed, retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
                attempt = attempt.saturating_add(1);
            }
        }
    }
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
        } = fetch_with_backoff(
            "CoinMarketCap",
            INITIAL_FETCH_ATTEMPTS,
            INITIAL_FETCH_BASE_BACKOFF,
            || client.current_price(),
        )
        .await?;
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
        } = fetch_with_backoff(
            "CoinGecko",
            INITIAL_FETCH_ATTEMPTS,
            INITIAL_FETCH_BASE_BACKOFF,
            || client.current_price(),
        )
        .await?;
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
        assert!(matches!(
            aggregator.current_snapshot(),
            Err(PriceOracleError::NoOraclesConfigured)
        ));
    }
}

#[cfg(test)]
mod future_timestamp_tests {
    use super::*;

    #[test]
    fn rejects_timestamp_beyond_skew() {
        let reported = UnixTimestamp::from_secs(1_000 + MAX_ORACLE_CLOCK_SKEW_SECS + 1);
        let err = reject_future_timestamp_at(reported, 1_000).unwrap_err();
        assert!(err.to_string().contains("check the system clock"));
        assert!(reject_future_timestamp_at(UnixTimestamp::from_secs(u64::MAX), 1_000).is_err());
    }

    #[test]
    fn accepts_timestamp_within_skew() {
        let at_limit = UnixTimestamp::from_secs(1_000 + MAX_ORACLE_CLOCK_SKEW_SECS);
        assert!(reject_future_timestamp_at(at_limit, 1_000).is_ok());
        assert!(reject_future_timestamp_at(UnixTimestamp::from_secs(500), 1_000).is_ok());
    }

    #[test]
    fn live_clock_check() {
        let now = UnixTimestamp::now().unwrap();
        assert!(reject_future_timestamp(now).is_ok());
        let future = UnixTimestamp::from_secs(now.as_secs() + MAX_ORACLE_CLOCK_SKEW_SECS + 10);
        assert!(reject_future_timestamp(future).is_err());
    }
}

#[cfg(test)]
mod zero_price_tests {
    use super::*;
    use irys_types::U256;

    #[test]
    fn rejects_zero_price() {
        assert!(reject_zero_price(Amount::new(U256::from(0_u64))).is_err());
    }

    #[test]
    fn accepts_nonzero_price() {
        assert!(reject_zero_price(Amount::new(U256::from(1_u64))).is_ok());
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "simpler tests")]
mod http_limit_tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    /// Serves one canned HTTP response, then closes; returns the URL.
    async fn serve_once(head: &'static str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut scratch = [0_u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut scratch).await;
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body).await;
            let _ = sock.shutdown().await;
        });
        url
    }

    #[tokio::test]
    async fn reads_body_within_cap() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
            b"{}".to_vec(),
        )
        .await;
        let resp = build_http_client().get(url).send().await.unwrap();
        assert_eq!(read_capped_body(resp).await.unwrap(), b"{}");
    }

    #[tokio::test]
    async fn rejects_streamed_body_over_cap() {
        let oversized = vec![b'x'; usize::try_from(MAX_ORACLE_RESPONSE_BYTES).unwrap() + 1];
        let url = serve_once("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", oversized).await;
        let resp = build_http_client().get(url).send().await.unwrap();
        assert!(read_capped_body(resp).await.is_err());
    }

    #[tokio::test]
    async fn rejects_declared_length_over_cap() {
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n",
            Vec::new(),
        )
        .await;
        let resp = build_http_client().get(url).send().await.unwrap();
        assert!(read_capped_body(resp).await.is_err());
    }
}

#[cfg(test)]
mod initial_fetch_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retries_until_success() {
        let calls = AtomicU32::new(0);
        let result = fetch_with_backoff("test", 5, Duration::from_millis(1), || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                eyre::bail!("transient")
            }
            Ok(7_u32)
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fails_after_all_attempts() {
        let calls = AtomicU32::new(0);
        let result: eyre::Result<u32> =
            fetch_with_backoff("test", 3, Duration::from_millis(1), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                eyre::bail!("down")
            })
            .await;
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("after 3 attempts") && err.contains("down"),
            "{err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
