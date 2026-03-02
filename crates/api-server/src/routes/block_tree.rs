use actix_web::{web, HttpResponse, ResponseError as _};
use awc::http::StatusCode;
use irys_types::serialization::{string_u128, string_u64};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, ApiState};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockAtHeight {
    pub block_hash: String,
    pub cumulative_diff: String,
    #[serde(with = "string_u128")]
    pub timestamp: u128,
    pub solution_hash: String,
    pub is_tip: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkInfo {
    #[serde(with = "string_u64")]
    pub height: u64,
    pub block_count: usize,
    pub blocks: Vec<BlockAtHeight>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTreeForksResponse {
    #[serde(with = "string_u64")]
    pub current_tip_height: u64,
    pub current_tip_hash: String,
    pub forks: Vec<ForkInfo>,
    pub total_fork_count: usize,
}

pub async fn get_block_tree_forks(state: web::Data<ApiState>) -> HttpResponse {
    let block_tree = state.block_tree.read();

    let tip_hash = block_tree.tip;
    let tip_block = match block_tree.get_block(&tip_hash) {
        Some(block) => block,
        None => {
            return ApiError::CustomWithStatus(
                "Failed to get the tip block".to_owned(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .error_response();
        }
    };

    let tip_height = tip_block.height;

    let mut forks: Vec<ForkInfo> = Vec::new();

    let min_height = tip_height.saturating_sub(100);

    for height in min_height..=tip_height {
        if let Some(blocks_at_height) = block_tree.get_hashes_for_height(height) {
            if blocks_at_height.len() > 1 {
                let mut blocks_info: Vec<BlockAtHeight> = Vec::new();

                for block_hash in blocks_at_height {
                    if let Some(block) = block_tree.get_block(block_hash) {
                        blocks_info.push(BlockAtHeight {
                            block_hash: block_hash.to_string(),
                            cumulative_diff: block.cumulative_diff.to_string(),
                            timestamp: block.timestamp.as_millis(),
                            solution_hash: block.solution_hash.to_string(),
                            is_tip: *block_hash == tip_hash,
                        });
                    }
                }

                blocks_info.sort_by(|a, b| b.cumulative_diff.cmp(&a.cumulative_diff));

                forks.push(ForkInfo {
                    height,
                    block_count: blocks_at_height.len(),
                    blocks: blocks_info,
                });
            }
        }
    }

    forks.sort_by(|a, b| b.height.cmp(&a.height));

    let total_fork_count = forks.len();

    HttpResponse::Ok().json(BlockTreeForksResponse {
        current_tip_height: tip_height,
        current_tip_hash: tip_hash.to_string(),
        forks,
        total_fork_count,
    })
}
