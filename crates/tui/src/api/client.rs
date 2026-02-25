use crate::{api::models::*, types::NodeUrl};
use backon::{ExponentialBuilder, Retryable as _};
use eyre::{Context as _, Result, eyre};
use reqwest::{Client, ClientBuilder};
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, warn};

#[derive(Debug, Clone, Copy)]
pub enum LedgerType {
    Publish,
    Submit,
}

impl LedgerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Publish => "Publish",
            Self::Submit => "Submit",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ChunkType {
    Data,
    Entropy,
}

impl ChunkType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Data => "Data",
            Self::Entropy => "Entropy",
        }
    }
}

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
}

impl ApiClient {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        // Reduced connection timeout from 2s to 1s after profiling: prevents UI freezing
        // during node discovery when multiple nodes are down. 1s provides good balance
        // between responsiveness and avoiding false negatives on slow networks.
        let client = ClientBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(timeout_secs))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self { client })
    }

    #[instrument(skip_all, fields(endpoint = %endpoint))]
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

        // Retry configuration tuned for interactive UI:
        // - 3 attempts: balances reliability vs UI responsiveness
        // - 100ms min delay: imperceptible to users
        // - 5s max: prevents long UI freezes
        // - Jitter: prevents thundering herd when multiple nodes fail
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

    pub async fn get_node_info(&self, node_url: &str) -> Result<NodeInfo> {
        self.get_node_info_cancellable(node_url, &CancellationToken::new())
            .await
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
            Err(e) => {
                debug!("Failed to fetch /v1/info from {}: {}", node_url, e);
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
        self.get_with_cancellation(node_url, "/peer-list", Some(cancel_token))
            .await
    }

    pub async fn get_all_partition_chunk_counts_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<PartitionChunkCounts> {
        // Silent fallback to empty ChunkCounts on error:
        // 1. Some nodes don't implement /storage endpoints (optional feature)
        // 2. Transient network errors shouldn't crash entire node display
        // 3. Better to show partial data than hide entire node
        let publish_0 = self
            .get_chunk_counts_cancellable(node_url, LedgerType::Publish, 0, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());
        let submit_0 = self
            .get_chunk_counts_cancellable(node_url, LedgerType::Submit, 0, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());
        let submit_1 = self
            .get_chunk_counts_cancellable(node_url, LedgerType::Submit, 1, cancel_token)
            .await
            .unwrap_or_else(|_| ChunkCounts::new());

        Ok(PartitionChunkCounts {
            publish_0,
            submit_0,
            submit_1,
        })
    }

    pub async fn get_total_chunk_offsets(&self, node_url: &str) -> Result<TotalChunkOffsets> {
        let info: NodeInfo = self
            .get_with_cancellation(node_url, "/info", None)
            .await
            .context("Failed to get node info for total chunk offsets")?;

        let current_height: u64 = info.height.parse().context("Failed to parse node height")?;

        // Block migration depth calculation: we query height - 6 because blocks need
        // 6 confirmations before data migration begins. This ensures chunk offsets
        // are stable and won't change due to chain reorganizations.
        let block_migration_depth = 6;
        if current_height < block_migration_depth {
            return Ok(TotalChunkOffsets::default());
        }

        let migrated_height = current_height - block_migration_depth;

        // Get the block at migrated height
        let block_endpoint = format!("/block/{}", migrated_height);
        let block: serde_json::Value = self
            .get_with_cancellation(node_url, &block_endpoint, None)
            .await
            .context("Failed to get block for total chunk offsets")?;

        // Extract totalChunks for Publish (ledgerId 0) and Submit (ledgerId 1)
        let mut total_offsets = TotalChunkOffsets::default();

        if let Some(data_ledgers) = block.get("dataLedgers").and_then(|v| v.as_array()) {
            for ledger in data_ledgers {
                if let (Some(ledger_id), Some(total_chunks_str)) = (
                    ledger.get("ledgerId").and_then(serde_json::Value::as_u64),
                    ledger.get("totalChunks").and_then(|v| v.as_str()),
                ) {
                    if let Ok(total_chunks) = total_chunks_str.parse::<u64>() {
                        let max_offset = if total_chunks > 0 {
                            Some(total_chunks - 1)
                        } else {
                            None
                        };

                        match ledger_id {
                            0 => total_offsets.publish = max_offset,
                            1 => total_offsets.submit = max_offset,
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(total_offsets)
    }

    async fn get_chunk_counts_cancellable(
        &self,
        node_url: &str,
        ledger: LedgerType,
        slot_index: usize,
        cancel_token: &CancellationToken,
    ) -> Result<ChunkCounts> {
        let ledger_str = ledger.as_str();
        let data_endpoint = format!(
            "/storage/intervals/{ledger_str}/{slot_index}/{}",
            ChunkType::Data.as_str()
        );
        let entropy_endpoint = format!(
            "/storage/intervals/{ledger_str}/{slot_index}/{}",
            ChunkType::Entropy.as_str()
        );

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

    pub async fn get_mining_info_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<MiningInfo> {
        self.get_with_cancellation(node_url, "/mining/info", Some(cancel_token))
            .await
    }

    pub async fn get_block_tree_forks_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<BlockTreeForksResponse> {
        self.get_with_cancellation(node_url, "/block-tree/forks", Some(cancel_token))
            .await
    }

    pub async fn get_node_config_cancellable(
        &self,
        node_url: &str,
        cancel_token: &CancellationToken,
    ) -> Result<NodeConfig> {
        self.get_with_cancellation(node_url, "/config", Some(cancel_token))
            .await
    }

    // NodeUrl-accepting methods for validated URLs

    pub async fn get_node_info_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<NodeInfo> {
        self.get_node_info_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_peer_list_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<PeerListResponse> {
        self.get_peer_list_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_mempool_status_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<MempoolStatus> {
        self.get_mempool_status_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_mining_info_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<MiningInfo> {
        self.get_mining_info_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_block_tree_forks_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<BlockTreeForksResponse> {
        self.get_block_tree_forks_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_node_config_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<NodeConfig> {
        self.get_node_config_cancellable(url.as_str(), cancel_token)
            .await
    }

    pub async fn get_all_partition_chunk_counts_url(
        &self,
        url: &NodeUrl,
        cancel_token: &CancellationToken,
    ) -> Result<PartitionChunkCounts> {
        self.get_all_partition_chunk_counts_cancellable(url.as_str(), cancel_token)
            .await
    }
}
