use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub total_chunk_offsets: TotalChunkOffsets,
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

#[derive(Debug, Clone, Default)]
pub struct TotalChunkOffsets {
    pub publish: Option<u64>,
    pub submit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolStatus {
    pub data_tx_count: usize,
    pub commitment_tx_count: usize,
    pub pending_chunks_count: usize,
    pub pending_pledges_count: usize,
    pub recent_valid_tx_count: usize,
    pub recent_invalid_tx_count: usize,
    pub data_tx_total_size: u64,
    pub config: Value, // Using Value since we don't need to parse the config
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningInfo {
    // Block info
    pub block_height: u64,
    pub block_hash: String,
    pub block_timestamp: u128,

    // Difficulty info
    pub current_difficulty: String,
    pub cumulative_difficulty: String,
    pub last_diff_adjustment_timestamp: u128,

    // Mining rewards
    pub miner_address: String,
    pub reward_address: String,
    pub reward_amount: String,

    // VDF info (includes vdf_difficulty and next_vdf_difficulty)
    pub vdf_limiter_info: Value, // Using Value since it's a complex nested structure
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
            total_chunk_offsets: TotalChunkOffsets::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockAtHeight {
    pub block_hash: String,
    pub cumulative_diff: String,
    pub timestamp: u128,
    pub solution_hash: String,
    pub is_tip: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkInfo {
    pub height: u64,
    pub block_count: usize,
    pub blocks: Vec<BlockAtHeight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTreeForksResponse {
    pub current_tip_height: u64,
    pub current_tip_hash: String,
    pub forks: Vec<ForkInfo>,
    pub total_fork_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    pub consensus: ConsensusConfig,
    pub mempool: MempoolConfig,
    pub vdf: VdfConfig,
    pub node: NodeBasicConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsensusConfig {
    pub chain_id: u64,
    pub chunk_size: u64,
    pub num_chunks_in_partition: u64,
    pub num_chunks_in_recall_range: u64,
    pub num_partitions_per_slot: u64,
    pub entropy_packing_iterations: u32,
    pub block_migration_depth: u32,
    pub block_tree_depth: u64,
    pub max_data_txs_per_block: u64,
    pub max_commitment_txs_per_block: u64,
    pub anchor_expiry_depth: u8,
    pub commitment_fee: u64,
    pub block_time: u64,
    pub num_blocks_in_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MempoolConfig {
    pub anchor_expiry_depth: u8,
    pub block_migration_depth: u32,
    pub max_data_txs_per_block: u64,
    pub max_commitment_txs_per_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VdfConfig {
    pub parallel_verification_thread_limit: usize,
    pub reset_frequency: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub max_allowed_vdf_fork_steps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeBasicConfig {
    pub node_mode: String,
    pub http_port: u16,
    pub p2p_port: u16,
}
