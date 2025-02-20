use serde::{Deserialize, Serialize};

use crate::*;

/// This is hardcoded here to be used just by C packing related staff as it is also harcoded right now in C sources
pub const CHUNK_SIZE: u64 = 256 * 1024;

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
    pub chunk_migration_depth: u32,
    /// Irys chain id
    pub irys_chain_id: u64,
}

impl StorageConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            irys_chain_id: config.irys_chain_id,
            chunk_size: config.chunk_size,
            num_chunks_in_partition: config.num_chunks_in_partition,
            num_chunks_in_recall_range: config.num_chunks_in_recall_range,
            num_partitions_in_slot: config.num_partitions_per_slot,
            miner_address: config.mining_key,
            min_writes_before_sync: config.num_writes_before_sync,
            // TODO: revert this back
            entropy_packing_iterations: 1_000, /* PACKING_SHA_1_5_S */
            chunk_migration_depth: config.chunk_migration_depth,
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
