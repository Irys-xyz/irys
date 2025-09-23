use crate::api::models::*;
use backon::{ExponentialBuilder, Retryable as _};
use eyre::{eyre, Context as _, Result};
use reqwest::{Client, ClientBuilder};
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, warn};

// Partition ledger names
const PUBLISH_LEDGER: &str = "Publish";
const SUBMIT_LEDGER: &str = "Submit";

// Chunk type identifiers for storage intervals
const DATA_CHUNK_TYPE: &str = "Data";
const ENTROPY_CHUNK_TYPE: &str = "Entropy";

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
}

impl ApiClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let client = ClientBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(1)) // Reduced from 2 to 1 second
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self { client })
    }

    #[instrument(skip(self, cancel_token), fields(endpoint = %endpoint))]
    async fn get_with_cancellation<T>(
        &self,
        url: &str,
        endpoint: &str,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.get_with_cancellation_internal(url, endpoint, cancel_token, true)
            .await
    }

    async fn get_with_cancellation_without_version<T>(
        &self,
        url: &str,
        endpoint: &str,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.get_with_cancellation_internal(url, endpoint, cancel_token, false)
            .await
    }

    async fn get_with_cancellation_internal<T>(
        &self,
        url: &str,
        endpoint: &str,
        cancel_token: Option<&CancellationToken>,
        use_version: bool,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        if let Ok(parsed_url) = url::Url::parse(url) {
            if !matches!(parsed_url.scheme(), "http" | "https") {
                return Err(eyre!("Invalid URL scheme: only http/https allowed"));
            }
        }

        let full_url = if use_version {
            format!("{}/v1{}", url.trim_end_matches('/'), endpoint)
        } else {
            format!("{}{}", url.trim_end_matches('/'), endpoint)
        };
        debug!("Making request to: {}", full_url);

        let retry_strategy = ExponentialBuilder::default()
            .with_max_times(3)
            .with_min_delay(Duration::from_millis(100))
            .with_max_delay(Duration::from_secs(5))
            .with_jitter();

        let client = self.client.clone();

        let request_future = async {
            (|| async {
                let response = client
                    .get(&full_url)
                    .send()
                    .await
                    .map_err(|e| eyre!("Request failed: {}", e))?;

                if !response.status().is_success() {
                    return Err(eyre!("Request failed with status: {}", response.status()));
                }

                Ok(response)
            })
            .retry(retry_strategy)
            .await
        };

        let response = if let Some(token) = cancel_token {
            select! {
                result = request_future => result?,
                _ = token.cancelled() => {
                    warn!("Request cancelled: {}", full_url);
                    return Err(eyre!("Request cancelled"));
                }
            }
        } else {
            request_future.await?
        };

        let body = response
            .text()
            .await
            .map_err(|e| eyre!("Failed to read response body: {}", e))?;

        serde_json::from_str(&body)
            .map_err(|e| eyre!("Failed to parse JSON response: {}. Body: {}", e, body))
    }

    pub async fn get_node_info_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<NodeInfo> {
        // Try v1 endpoint first, fall back to non-versioned endpoint
        match self
            .get_with_cancellation(node_url, "/info", Some(cancel_token))
            .await
        {
            Ok(info) => Ok(info),
            Err(_) => {
                // Fallback: Try without /v1 prefix for older/remote nodes
                self.get_with_cancellation_without_version(node_url, "/info", Some(cancel_token))
                    .await
            }
        }
    }

    pub async fn get_peer_list_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<PeerListResponse> {
        self.get_with_cancellation(node_url, "/peer_list", Some(cancel_token))
            .await
    }

    pub async fn get_all_partition_chunk_counts_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<PartitionChunkCounts> {
        let publish_0 = self
            .get_chunk_counts_cancellable(node_url, PUBLISH_LEDGER, 0, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());
        let submit_0 = self
            .get_chunk_counts_cancellable(node_url, SUBMIT_LEDGER, 0, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());
        let submit_1 = self
            .get_chunk_counts_cancellable(node_url, SUBMIT_LEDGER, 1, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());

        Ok(PartitionChunkCounts {
            publish_0,
            submit_0,
            submit_1,
        })
    }

    async fn get_chunk_counts_cancellable(
        &self,
        node_url: &str,
        ledger: &str,
        slot_index: usize,
        cancel_token: &CancellationToken,
    ) -> Result<ChunkCounts> {
        let data_endpoint =
            format!("/observability/storage/intervals/{ledger}/{slot_index}/{DATA_CHUNK_TYPE}");
        let entropy_endpoint =
            format!("/observability/storage/intervals/{ledger}/{slot_index}/{ENTROPY_CHUNK_TYPE}");

        let data_intervals: StorageIntervalsResponse = self
            .get_with_cancellation(node_url, &data_endpoint, Some(cancel_token))
            .await?;
        let packed_intervals: StorageIntervalsResponse = self
            .get_with_cancellation(node_url, &entropy_endpoint, Some(cancel_token))
            .await?;

        let data_count = ChunkCounts::from_intervals(&data_intervals.intervals);
        let packed_count = ChunkCounts::from_intervals(&packed_intervals.intervals);

        Ok(ChunkCounts {
            data: data_count,
            packed: packed_count,
        })
    }

    pub async fn get_mempool_status_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<MempoolStatus> {
        self.get_with_cancellation(node_url, "/mempool/status", Some(cancel_token))
            .await
    }
}
