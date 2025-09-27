use irys_types::H256;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GenesisResponse {
    pub genesis_block_hash: String,
    pub height: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AnchorResponse {
    pub anchor: H256,
    pub block_height: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PriceResponse {
    pub price: String,
    pub perm_fee: String,
    pub term_fee: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NetworkConfigResponse {
    pub chunk_size: String,
    pub chain_id: String,
    pub num_chunks_in_partition: String,
    pub num_chunks_in_recall_range: String,
    pub num_partitions_per_slot: String,
    pub entropy_packing_iterations: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChainHeightResponse {
    pub height: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct ChunkInterval {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StorageIntervalsResponse {
    pub ledger: String,
    pub slot_index: usize,
    pub chunk_type: String,
    pub intervals: Vec<ChunkInterval>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TransactionStatusResponse {
    pub status: String,
    pub block_height: Option<u64>,
    pub confirmations: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LedgerSummary {
    pub node_id: String,
    pub ledger_type: String,
    pub assignment_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeInfo {
    pub version: String,
    pub network: String,
}

#[derive(Debug, Clone)]
#[expect(dead_code)]
pub(crate) struct StorageStatus {
    pub node_name: String,
    pub publish_data: usize,
    pub publish_packed: usize,
    pub submit0_data: usize,
    pub submit0_packed: usize,
    pub submit1_packed: usize,
}

impl StorageStatus {
    pub(crate) fn total_chunks(&self) -> usize {
        self.publish_data
            + self.publish_packed
            + self.submit0_data
            + self.submit0_packed
            + self.submit1_packed
    }
}

#[derive(Debug)]
pub(crate) struct NodeStorageCounts {
    pub publish_slots: Vec<SlotCounts>,
    pub submit_slots: Vec<SlotCounts>,
    #[expect(dead_code)]
    pub node_url: String,
}

#[derive(Debug)]
pub(crate) struct SlotCounts {
    pub slot_index: usize,
    pub data_chunks: usize,
    pub packed_chunks: usize,
}

#[derive(Debug)]
#[expect(dead_code)]
pub(crate) struct DetailedSyncValidation {
    pub node_name: String,
    pub expected_data_chunks: usize,
    pub expected_packed_chunks_slot0: usize,
    pub expected_packed_chunks_slot1: usize,
    pub is_fully_synced: bool,
    pub sync_details: String,
}

// Structures for future Phase 2 implementation
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PartitionAssignment {
    pub ledger_id: Option<u32>,
    pub slot_index: usize,
    pub partition_index: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EpochInfo {
    pub current_epoch: u64,
    pub partition_assignments: Vec<PartitionAssignment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MiningRequest {
    pub count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MiningResponse {
    pub blocks_mined: u32,
    pub latest_height: u64,
}
