use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json};
use irys_domain::get_canonical_chain;
use irys_types::{
    serialization::{string_u128, string_u64},
    VDFLimiterInfo,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MiningInfo {
    // Block info
    #[serde(with = "string_u64")]
    pub block_height: u64,
    pub block_hash: String,
    #[serde(with = "string_u128")]
    pub block_timestamp: u128,

    // Difficulty info
    pub current_difficulty: String,
    pub cumulative_difficulty: String,
    #[serde(with = "string_u128")]
    pub last_diff_adjustment_timestamp: u128,

    // Mining rewards
    pub miner_address: String,
    pub reward_address: String,
    pub reward_amount: String,

    // VDF info (includes vdf_difficulty and next_vdf_difficulty)
    pub vdf_limiter_info: VDFLimiterInfo,
}

pub async fn get_mining_info(app_state: Data<ApiState>) -> Result<Json<MiningInfo>, ApiError> {
    let canonical_chain = get_canonical_chain(app_state.block_tree.clone())
        .await
        .map_err(ApiError::canonical_chain_error)?;

    let latest_block_entry = canonical_chain
        .0
        .last()
        .ok_or(ApiError::EmptyCanonicalChain)?;

    // Get the full block header from the block tree
    let block_tree = app_state.block_tree.read();
    let header = block_tree
        .get_block(&latest_block_entry.block_hash)
        .ok_or_else(|| ApiError::BlockNotFound {
            block_hash: format!("{:?}", latest_block_entry.block_hash),
        })?;

    Ok(Json(MiningInfo {
        block_height: header.height,
        block_hash: format!("{:?}", header.block_hash),
        block_timestamp: header.timestamp.as_millis(),
        current_difficulty: header.diff.to_string(),
        cumulative_difficulty: header.cumulative_diff.to_string(),
        last_diff_adjustment_timestamp: header.last_diff_timestamp.as_millis(),
        miner_address: format!("{:?}", header.miner_address),
        reward_address: format!("{:?}", header.reward_address),
        reward_amount: header.reward_amount.to_string(),
        vdf_limiter_info: header.vdf_limiter_info.clone(),
    }))
}
