use serde::{Deserialize, Serialize};

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
    /// Number of sha256 iterations required to pack a chunk
    pub entropy_packing_iterations: u32,
    /// Number of confirmations before storing tx data in `StorageModule`s
    pub num_confirmations_for_finality: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            chunk_size: CONFIG.chunk_size,
            num_chunks_in_partition: CONFIG.num_chunks_in_partition,
            num_chunks_in_recall_range: CONFIG.num_chunks_in_recall_range,
            num_partitions_in_slot: CONFIG.num_partitions_per_slot,
            miner_address: Address::random(),
            min_writes_before_sync: CONFIG.num_writes_before_sync,
            // TODO: revert this back
            entropy_packing_iterations: 1_000, /* PACKING_SHA_1_5_S */
            num_confirmations_for_finality: CONFIG.num_confirmations_for_finality,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Public variant of StorageConfig, containing network-wide parameters
/// Primarily used for testing clients, so we don't have to manually sync parameters
/// note: chain ID is not included for now as that's still a constant
/// once we parameterize that we'll put it in here.
#[serde(rename_all = "camelCase")]
pub struct PublicStorageConfig {
    /// Size of each chunk in bytes
    #[serde(with = "string_u64")]
    pub chunk_size: u64,
    /// Number of chunks in a partition
    #[serde(with = "string_u64")]
    pub num_chunks_in_partition: u64,
    /// Number of chunks in a recall range
    #[serde(with = "string_u64")]
    pub num_chunks_in_recall_range: u64,
    /// Number of partition replicas in a ledger slot
    #[serde(with = "string_u64")]
    pub num_partitions_in_slot: u64,
    /// Number of sha256 iterations required to pack a chunk
    pub entropy_packing_iterations: u32,
}

impl From<StorageConfig> for PublicStorageConfig {
    fn from(value: StorageConfig) -> Self {
        let StorageConfig {
            chunk_size,
            num_chunks_in_partition,
            num_chunks_in_recall_range,
            num_partitions_in_slot,
            entropy_packing_iterations,
            ..
        } = value;
        PublicStorageConfig {
            chunk_size,
            num_chunks_in_partition,
            num_chunks_in_recall_range,
            num_partitions_in_slot,
            entropy_packing_iterations,
        }
    }
}
