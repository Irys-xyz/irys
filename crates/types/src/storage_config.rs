use crate::*;

/// Protocol storage sizing configuration
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Size of each chunk in bytes
    pub chunk_size: u64,
    /// Number of chunks in a partition
    pub num_chunks_in_partition: u64,
    /// Number of chunks in a recall range
    pub num_chunks_in_recall_range: u64,
    /// Number of partition replicas in a ledger slot
    pub num_partitions_in_slot: u64,
    /// Local mining address
    pub miner_address: Address,
    /// Number of writes before a StorageModule syncs to disk
    pub min_writes_before_sync: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            chunk_size: CHUNK_SIZE,
            num_chunks_in_partition: NUM_CHUNKS_IN_PARTITION,
            num_chunks_in_recall_range: NUM_CHUNKS_IN_RECALL_RANGE,
            num_partitions_in_slot: NUM_PARTITIONS_PER_SLOT,
            miner_address: Address::random(),
            min_writes_before_sync: NUM_WRITES_BEFORE_SYNC,
        }
    }
}
