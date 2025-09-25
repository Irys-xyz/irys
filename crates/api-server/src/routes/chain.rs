use crate::error::ApiError;
use crate::ApiState;
use actix_web::web::{Data, Json};
use irys_domain::get_canonical_chain;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct ChainHeight {
    height: u64,
}

pub async fn get_height(app_state: Data<ApiState>) -> Result<Json<ChainHeight>, ApiError> {
    let canonical_chain = get_canonical_chain(app_state.block_tree.clone())
        .await
        .map_err(ApiError::canonical_chain_error)?;

    let height = canonical_chain
        .0
        .last()
        .map(|block| block.height)
        .ok_or(ApiError::EmptyCanonicalChain)?;

    Ok(Json(ChainHeight { height }))
}
