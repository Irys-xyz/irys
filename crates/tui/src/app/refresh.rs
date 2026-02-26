use crate::{
    api::{client::ApiClient, models::*},
    app::state::{AppState, NodeState},
    db::db_queue::DatabaseWriter,
    types::NodeUrl,
};
use eyre::Result;
use futures::future::join_all;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const INITIAL_REFRESH_TIMEOUT_SECS: u64 = 3;
const CHUNK_SIZE_KB: f64 = 32.0;
const KB_TO_GB: f64 = 1024.0 * 1024.0;

/// Temporary structure to hold node refresh data
#[derive(Default)]
pub struct NodeData {
    pub info: Option<NodeInfo>,
    pub is_reachable: bool,
    pub response_time_ms: Option<u64>,
    pub chain_height: Option<ChainHeight>,
    pub peers: Option<PeerListResponse>,
    pub mempool_status: Option<MempoolStatus>,
}

pub struct RefreshManager;

impl RefreshManager {
    pub async fn refresh_data(
        state: &mut AppState,
        api_client: &ApiClient,
        refresh_cancel_token: &mut CancellationToken,
        database_writer: &Option<DatabaseWriter>,
    ) -> Result<()> {
        // Cancel any existing refresh operation
        refresh_cancel_token.cancel();

        // Small delay to ensure cancellation takes effect
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Create a new cancellation token for this refresh
        *refresh_cancel_token = CancellationToken::new();
        let cancel_token = refresh_cancel_token.clone();

        // Mark refresh as in progress
        state.is_refreshing = true;
        state.time_since_last_refresh = std::time::Duration::from_secs(0);

        let urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();

        // Fetch all nodes concurrently with reduced timeout
        let fetch_futures: Vec<_> = urls
            .iter()
            .map(|url| {
                let url = url.clone();
                let api_client = api_client.clone();
                let cancel_token = cancel_token.clone();

                async move {
                    let result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(INITIAL_REFRESH_TIMEOUT_SECS),
                        Self::fetch_node_data(&url, &api_client, &cancel_token),
                    )
                    .await;

                    match result {
                        Ok(data) => (url.clone(), data),
                        Err(_) => (url.clone(), Self::create_timeout_data()),
                    }
                }
            })
            .collect();

        let results = join_all(fetch_futures).await;

        // Update state with results
        for (url, node_data) in results {
            if cancel_token.is_cancelled() {
                break;
            }

            // Record to database if recording is enabled
            if state.is_recording
                && let Some(db_writer) = database_writer
            {
                Self::record_node_data(&url, &node_data, db_writer, state);
            }

            if let Some(node_state) = state.nodes.get_mut(&url) {
                Self::update_node_state(node_state, node_data);
            }
        }

        // Mark refresh as complete
        state.is_refreshing = false;

        Ok(())
    }

    pub async fn refresh_observability_data(
        state: &mut AppState,
        api_client: &ApiClient,
    ) -> Result<()> {
        // Only fetch observability data for reachable nodes
        let urls: Vec<NodeUrl> = state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(chunk_counts) = api_client
                .get_all_partition_chunk_counts_url(&url, &cancel_token)
                .await
                && let Some(node_state) = state.nodes.get_mut(&url)
            {
                node_state.metrics.chunk_counts = chunk_counts;
            }

            // Fetch total chunk offsets from blockchain
            if let Ok(total_offsets) = api_client.get_total_chunk_offsets(url.as_str()).await
                && let Some(node_state) = state.nodes.get_mut(&url)
            {
                node_state.metrics.total_chunk_offsets = total_offsets;
            }
        }

        Ok(())
    }

    pub async fn refresh_mempool_data(
        state: &mut AppState,
        api_client: &ApiClient,
        database_writer: &Option<DatabaseWriter>,
    ) -> Result<()> {
        // Only fetch mempool data for reachable nodes
        let urls: Vec<NodeUrl> = state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(mempool_status) = api_client.get_mempool_status_url(&url, &cancel_token).await
            {
                // Record to database if recording is enabled
                if state.is_recording
                    && let Some(db_writer) = database_writer
                {
                    let raw_json = serde_json::to_string(&mempool_status).unwrap_or_default();
                    if let Err(e) = db_writer.record_mempool_status(
                        url.as_str().to_string(),
                        mempool_status.clone(),
                        raw_json,
                    ) {
                        tracing::error!("Failed to record mempool status to database: {}", e);
                    }
                }

                if let Some(node_state) = state.nodes.get_mut(&url) {
                    node_state.mempool_status = Some(mempool_status);
                }
            }
        }

        Ok(())
    }

    pub async fn refresh_mining_data(
        state: &mut AppState,
        api_client: &ApiClient,
        database_writer: &Option<DatabaseWriter>,
    ) -> Result<()> {
        // Only fetch mining data for reachable nodes
        let urls: Vec<NodeUrl> = state
            .nodes
            .iter()
            .filter(|(_, node)| node.is_reachable)
            .map(|(url, _)| url.clone())
            .collect();

        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(mining_info) = api_client.get_mining_info_url(&url, &cancel_token).await {
                // Record to database if recording is enabled
                if state.is_recording
                    && let Some(db_writer) = database_writer
                {
                    let raw_json = serde_json::to_string(&mining_info).unwrap_or_default();
                    if let Err(e) = db_writer.record_mining_info(
                        url.as_str().to_string(),
                        mining_info.clone(),
                        raw_json,
                    ) {
                        tracing::error!("Failed to record mining info to database: {}", e);
                    }
                }

                if let Some(node_state) = state.nodes.get_mut(&url) {
                    node_state.mining_info = Some(mining_info);
                }
            }
        }

        Ok(())
    }

    pub async fn refresh_forks_data(
        state: &mut AppState,
        api_client: &ApiClient,
        database_writer: &Option<DatabaseWriter>,
    ) -> Result<()> {
        let urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(fork_info) = api_client
                .get_block_tree_forks_url(&url, &cancel_token)
                .await
            {
                // Record to database if recording is enabled
                if state.is_recording
                    && let Some(db_writer) = database_writer
                {
                    let raw_json = serde_json::to_string(&fork_info).unwrap_or_default();

                    // Calculate fork metrics
                    let fork_count = fork_info.forks.len();
                    let max_fork_depth =
                        fork_info.forks.iter().map(|f| f.height).max().unwrap_or(0);
                    let total_forked_blocks: usize =
                        fork_info.forks.iter().map(|f| f.block_count).sum();

                    if let Err(e) = db_writer.record_fork_data(
                        url.as_str().to_string(),
                        fork_info.current_tip_height,
                        fork_info.current_tip_hash.clone(),
                        fork_count,
                        max_fork_depth,
                        total_forked_blocks,
                        raw_json,
                    ) {
                        tracing::error!("Failed to record fork data to database: {}", e);
                    }
                }

                if let Some(node_state) = state.nodes.get_mut(&url) {
                    node_state.fork_info = Some(fork_info);
                }
            }
        }

        Ok(())
    }

    pub async fn refresh_config_data(state: &mut AppState, api_client: &ApiClient) -> Result<()> {
        let urls: Vec<NodeUrl> = state.nodes.keys().cloned().collect();
        let cancel_token = CancellationToken::new();

        for url in urls {
            if let Ok(config) = api_client.get_node_config_url(&url, &cancel_token).await
                && let Some(node_state) = state.nodes.get_mut(&url)
            {
                node_state.config = Some(config);
            }
        }

        Ok(())
    }

    async fn fetch_node_data(
        url: &NodeUrl,
        api_client: &ApiClient,
        cancel_token: &CancellationToken,
    ) -> NodeData {
        if cancel_token.is_cancelled() {
            return Self::create_timeout_data();
        }

        let start_time = Instant::now();
        let mut data = NodeData::default();

        match api_client.get_node_info_url(url, cancel_token).await {
            Ok(info) => {
                data.info = Some(info);
                data.is_reachable = true;
                data.response_time_ms = Some(start_time.elapsed().as_millis() as u64);
            }
            Err(_) => {
                data.is_reachable = false;
                return data;
            }
        }

        // Fetch only basic data for node health (info already fetched above)
        // Only fetch peer list for basic connectivity info
        if let Ok(p) = api_client.get_peer_list_url(url, cancel_token).await {
            data.peers = Some(p);
        }

        if let Some(ref info) = data.info
            && let Ok(height) = info.height.parse::<u64>()
        {
            data.chain_height = Some(ChainHeight { height });
        }

        data
    }

    fn create_timeout_data() -> NodeData {
        NodeData {
            is_reachable: false,
            ..Default::default()
        }
    }

    fn update_node_state(node_state: &mut NodeState, data: NodeData) {
        node_state.metrics.info = data.info;
        node_state.is_reachable = data.is_reachable;
        node_state.response_time_ms = data.response_time_ms;

        if node_state.is_reachable {
            if let Some(height) = data.chain_height {
                node_state.metrics.chain_height = Some(height);
            }
            if let Some(peers) = data.peers {
                node_state.peers = peers.clone();
                node_state.metrics.peer_count = peers.len();
            }
            if let Some(mempool) = data.mempool_status {
                node_state.mempool_status = Some(mempool);
            }
            // Don't update chunk_counts here - only fetch when needed for DataSync view
        }

        node_state.last_updated = chrono::Utc::now();

        if let Some(response_time) = node_state.response_time_ms {
            node_state.metrics.add_response_time(response_time);
        }
    }

    // Async database writes via queue pattern:
    // UI thread enqueues, background thread writes to SQLite
    // Prevents UI stuttering during recording of 100+ nodes
    fn record_node_data(
        url: &NodeUrl,
        node_data: &NodeData,
        db_writer: &DatabaseWriter,
        state: &AppState,
    ) {
        if let Some(ref info) = node_data.info {
            let raw_json = serde_json::to_string(&info).unwrap_or_default();
            if let Err(e) =
                db_writer.record_node_info(url.as_str().to_string(), info.clone(), raw_json)
            {
                tracing::error!("Failed to record node info to database: {}", e);
            }
        }

        // Also record data sync metrics if available
        if let Some(ref info) = node_data.info
            && let Ok(sync_height) = info.current_sync_height.to_string().parse::<u64>()
        {
            // Get chunk counts from node state if available
            let (total_data_chunks, total_packed_chunks) =
                if let Some(node_state) = state.nodes.get(url) {
                    let data_chunks = node_state.metrics.chunk_counts.publish_0.data
                        + node_state.metrics.chunk_counts.submit_0.data
                        + node_state.metrics.chunk_counts.submit_1.data;
                    let packed_chunks = node_state.metrics.chunk_counts.publish_0.packed
                        + node_state.metrics.chunk_counts.submit_0.packed
                        + node_state.metrics.chunk_counts.submit_1.packed;
                    (data_chunks, packed_chunks)
                } else {
                    (0, 0)
                };

            // Calculate sync progress (simplified)
            let sync_progress = if let Ok(chain_height) = info.height.parse::<u64>() {
                if chain_height > 0 {
                    (sync_height as f64 / chain_height as f64) * 100.0
                } else {
                    0.0
                }
            } else {
                0.0
            };

            let raw_json = format!(
                r#"{{"sync_height":{}, "total_data_chunks":{}, "total_packed_chunks":{}, "sync_progress":{}}}"#,
                sync_height, total_data_chunks, total_packed_chunks, sync_progress
            );

            if let Err(e) = db_writer.record_data_sync(
                url.as_str().to_string(),
                sync_height,
                total_data_chunks,
                total_packed_chunks,
                sync_progress,
                0.0, // sync_speed_mbps - would need to calculate from historical data
                raw_json,
            ) {
                tracing::error!("Failed to record data sync to database: {}", e);
            }
        }

        // Record overall metrics
        if let Some(node_state) = state.nodes.get(url) {
            let avg_response_time = if !node_state.metrics.response_times.is_empty() {
                node_state.metrics.response_times.iter().sum::<u64>()
                    / node_state.metrics.response_times.len() as u64
            } else {
                0
            };

            let total_chunks = node_state.metrics.chunk_counts.publish_0.data
                + node_state.metrics.chunk_counts.submit_0.data
                + node_state.metrics.chunk_counts.submit_1.data
                + node_state.metrics.chunk_counts.publish_0.packed
                + node_state.metrics.chunk_counts.submit_0.packed
                + node_state.metrics.chunk_counts.submit_1.packed;

            let storage_used_gb = (total_chunks as f64 * CHUNK_SIZE_KB) / KB_TO_GB;

            let raw_json = format!(
                r#"{{"uptime_percentage":{}, "avg_response_time_ms":{}, "error_count":{}, "total_chunks":{}, "storage_used_gb":{}}}"#,
                node_state.metrics.uptime_percentage,
                avg_response_time,
                node_state.metrics.error_count,
                total_chunks,
                storage_used_gb
            );

            if let Err(e) = db_writer.record_metrics(
                url.as_str().to_string(),
                node_state.metrics.uptime_percentage,
                avg_response_time,
                node_state.metrics.error_count,
                total_chunks,
                storage_used_gb,
                raw_json,
            ) {
                tracing::error!("Failed to record metrics to database: {}", e);
            }
        }
    }
}
