use irys_types::{partition::PartitionAssignment, H256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GenesisResponse {
    pub genesis_block_hash: String,
    pub height: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnchorResponse {
    pub block_hash: H256,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PriceResponse {
    pub perm_fee: String,
    pub term_fee: String,
    pub ledger: u32,
    pub bytes: String,
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

/// Structures for partition assignment verification
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PartitionAssignmentsResponse {
    pub node_id: String,
    pub assignments: Vec<PartitionAssignment>,
    pub epoch_height: u64,
    pub assignment_status: AssignmentStatus,
    pub hash_analysis: HashAnalysis,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum AssignmentStatus {
    FullyAssigned,
    PartiallyAssigned { assigned: usize, total: usize },
    Unassigned,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HashAnalysis {
    pub total_hashes: usize,
    pub unique_hashes: usize,
    pub zero_hashes: usize,
    pub duplicate_hashes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EpochInfoResponse {
    pub current_epoch: u64,
    pub epoch_block_height: u64,
    pub total_active_partitions: usize,
    pub unassigned_partitions: usize,
}

/// Structures for slot replica verification
#[derive(Debug, Clone)]
pub(crate) struct SlotReplicaInfo {
    pub slot_index: usize,
    pub ledger: irys_types::DataLedger,
    pub replicas: Vec<SlotReplica>,
    pub replica_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SlotReplica {
    pub node_address: String,
}

#[derive(Debug)]
pub(crate) struct SlotReplicaSummary {
    pub total_slots: usize,
    pub fully_replicated_slots: usize,
    pub under_replicated_slots: usize,
    pub over_replicated_slots: usize,
    pub expected_replicas: usize,
    pub slots: Vec<SlotReplicaInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ChunkCountsResponse {
    pub ledger: String,
    pub slot_index: usize,
    pub data_chunks: usize,
    pub packed_chunks: usize,
}

/// Types for assignment-based validation
#[derive(Debug, Clone)]
pub(crate) struct NodeAssignmentInfo {
    /// Slot indices assigned to Publish ledger
    pub publish_slots: Vec<usize>,
    /// Slot indices assigned to Submit ledger
    pub submit_slots: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct NodeExpectedStorage {
    /// Expected data chunks per slot (slot_index -> chunk_count)
    pub expected_publish_data: BTreeMap<usize, usize>,
    /// Expected packed chunks per slot (slot_index -> chunk_count)
    pub expected_publish_packed: BTreeMap<usize, usize>,
    /// Expected data chunks per slot (slot_index -> chunk_count)
    pub expected_submit_data: BTreeMap<usize, usize>,
    /// Expected packed chunks per slot (slot_index -> chunk_count)
    pub expected_submit_packed: BTreeMap<usize, usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct NodeSyncValidationResult {
    pub node_name: String,
    pub is_synced: bool,
    pub details: String,
    pub assignment_info: NodeAssignmentInfo,
}
