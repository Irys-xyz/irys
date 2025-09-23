use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub version: String,
    #[serde(rename = "peerCount")]
    pub peer_count: u32,
    #[serde(rename = "chainId")]
    pub chain_id: String,
    pub height: String,
    #[serde(rename = "blockHash")]
    pub block_hash: String,
    #[serde(rename = "blockIndexHeight")]
    pub block_index_height: String,
    #[serde(rename = "blockIndexHash")]
    pub block_index_hash: String,
    #[serde(rename = "pendingBlocks")]
    pub pending_blocks: String,
    #[serde(rename = "isSyncing")]
    pub is_syncing: bool,
    #[serde(rename = "currentSyncHeight")]
    pub current_sync_height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInfo {
    pub peering_tcp_addr: String,
    pub peer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub gossip: String,
    pub api: String,
    pub execution: ExecutionInfo,
}

pub type PeerListResponse = Vec<PeerInfo>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainHeight {
    pub height: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StorageIntervalsResponse {
    pub ledger: String,
    pub slot_index: usize,
    pub chunk_type: String,
    pub intervals: Vec<ChunkInterval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChunkInterval {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChunkCounts {
    pub data: u64,
    pub packed: u64,
}

impl ChunkCounts {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn from_intervals(intervals: &[ChunkInterval]) -> u64 {
        intervals
            .iter()
            .map(|interval| (interval.end - interval.start + 1) as u64)
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct NodeMetrics {
    pub info: Option<NodeInfo>,
    pub chain_height: Option<ChainHeight>,
    pub peer_count: usize,
    pub chunk_counts: PartitionChunkCounts,
    pub last_updated: DateTime<Utc>,
    pub response_times: Vec<u64>,
    pub error_count: u32,
    pub uptime_percentage: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PartitionChunkCounts {
    pub publish_0: ChunkCounts,
    pub submit_0: ChunkCounts,
    pub submit_1: ChunkCounts,
}

impl PartitionChunkCounts {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for NodeMetrics {
    fn default() -> Self {
        Self {
            info: None,
            chain_height: None,
            peer_count: 0,
            chunk_counts: PartitionChunkCounts::default(),
            last_updated: Utc::now(),
            response_times: Vec::new(),
            error_count: 0,
            uptime_percentage: 100.0,
        }
    }
}

impl NodeMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_response_time(&mut self, time_ms: u64) {
        const RESPONSE_TIME_WINDOW_SIZE: usize = 50;
        self.response_times.push(time_ms);
        if self.response_times.len() > RESPONSE_TIME_WINDOW_SIZE {
            self.response_times.remove(0);
        }
    }

    pub fn average_response_time(&self) -> Option<f64> {
        if self.response_times.is_empty() {
            None
        } else {
            Some(self.response_times.iter().sum::<u64>() as f64 / self.response_times.len() as f64)
        }
    }

    pub fn is_healthy(&self) -> bool {
        const MAX_HEALTHY_RESPONSE_TIME_MS: f64 = 5000.0;

        self.info.is_some()
            && self.chain_height.is_some()
            && self
                .average_response_time()
                .is_some_and(|t| t < MAX_HEALTHY_RESPONSE_TIME_MS)
    }
}
