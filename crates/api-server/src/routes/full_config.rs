use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::config::ConsensusConfig;
use serde::{Deserialize, Serialize};

use crate::ApiState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicFullConfig {
    pub consensus: ConsensusConfig,
    pub mempool: PublicMempoolConfig,
    pub vdf: PublicVdfConfig,
    pub node: PublicNodeConfig,
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
            consensus: config.consensus.clone(),
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
