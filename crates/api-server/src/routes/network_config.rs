use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::serialization::string_u64;
use serde::{Deserialize, Serialize};

/// Public variant of StorageConfig, containing network-wide parameters
/// Primarily used for testing clients, so we don't have to manually sync parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicStorageConfig {
    /// Size of each chunk in bytes
    #[serde(with = "string_u64")]
    pub chunk_size: u64,
    /// Size of each chunk in bytes
    #[serde(with = "string_u64")]
    pub chain_id: u64,
    /// Number of chunks in a partition
    #[serde(with = "string_u64")]
    pub num_chunks_in_partition: u64,
    /// Number of chunks in a recall range
    #[serde(with = "string_u64")]
    pub num_chunks_in_recall_range: u64,
    /// Number of partition replicas in a ledger slot
    #[serde(with = "string_u64")]
    pub num_partitions_per_slot: u64,
    /// Number of sha256 iterations required to pack a chunk
    pub entropy_packing_iterations: u32,
}

use crate::ApiState;

pub async fn get_network_config(state: web::Data<ApiState>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(PublicStorageConfig {
            chunk_size: state.config.consensus.chunk_size,
            chain_id: state.config.consensus.chunk_size,
            num_chunks_in_partition: state.config.consensus.num_chunks_in_partition,
            num_chunks_in_recall_range: state.config.consensus.num_chunks_in_recall_range,
            num_partitions_per_slot: state.config.consensus.num_partitions_per_slot,
            entropy_packing_iterations: state.config.consensus.entropy_packing_iterations,
        })
}
