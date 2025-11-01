use actix_web::{http::header::ContentType, web, HttpResponse};
use serde::{Deserialize, Serialize};

use crate::ApiState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicFullConfig {
    pub consensus: PublicConsensusConfig,
    pub mempool: PublicMempoolConfig,
    pub vdf: PublicVdfConfig,
    pub node: PublicNodeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicConsensusConfig {
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
pub struct PublicMempoolConfig {
    pub anchor_expiry_depth: u8,
    pub block_migration_depth: u32,
    pub max_data_txs_per_block: u64,
    pub max_commitment_txs_per_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicVdfConfig {
    pub parallel_verification_thread_limit: usize,
    pub reset_frequency: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub max_allowed_vdf_fork_steps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicNodeConfig {
    pub node_mode: String,
    pub http_port: u16,
    pub p2p_port: u16,
}

pub async fn get_full_config(state: web::Data<ApiState>) -> HttpResponse {
    let config = &state.config;

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(PublicFullConfig {
            // TODO: we need to change how we serialize some of these fields (u64s need to use the u64_string serializer)
            // TODO: why not just.. expose the entire consensus config? it's meant to be fully public/shared anyway
            consensus: PublicConsensusConfig {
                chain_id: config.consensus.chain_id,
                chunk_size: config.consensus.chunk_size,
                num_chunks_in_partition: config.consensus.num_chunks_in_partition,
                num_chunks_in_recall_range: config.consensus.num_chunks_in_recall_range,
                num_partitions_per_slot: config.consensus.num_partitions_per_slot,
                entropy_packing_iterations: config.consensus.entropy_packing_iterations,
                block_migration_depth: config.consensus.block_migration_depth,
                block_tree_depth: config.consensus.block_tree_depth,
                max_data_txs_per_block: config.consensus.mempool.max_data_txs_per_block,
                max_commitment_txs_per_block: config.consensus.mempool.max_commitment_txs_per_block,
                anchor_expiry_depth: config.consensus.mempool.anchor_expiry_depth,
                commitment_fee: config.consensus.mempool.commitment_fee,
                block_time: config.consensus.difficulty_adjustment.block_time,
                num_blocks_in_epoch: config.consensus.epoch.num_blocks_in_epoch,
            },
            mempool: PublicMempoolConfig {
                anchor_expiry_depth: config.consensus.mempool.anchor_expiry_depth,
                block_migration_depth: config.consensus.block_migration_depth,
                max_data_txs_per_block: config.consensus.mempool.max_data_txs_per_block,
                max_commitment_txs_per_block: config.consensus.mempool.max_commitment_txs_per_block,
            },
            vdf: PublicVdfConfig {
                parallel_verification_thread_limit: config.vdf.parallel_verification_thread_limit,
                reset_frequency: config.vdf.reset_frequency,
                num_checkpoints_in_vdf_step: config.vdf.num_checkpoints_in_vdf_step,
                max_allowed_vdf_fork_steps: config.vdf.max_allowed_vdf_fork_steps,
            },
            node: PublicNodeConfig {
                node_mode: format!("{:?}", config.node_config.node_mode),
                http_port: config.node_config.http.bind_port,
                p2p_port: config.node_config.gossip.bind_port,
            },
        })
}
