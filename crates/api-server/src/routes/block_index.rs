use crate::{error::ApiError, ApiState};
use actix_web::web::{self, Json};
use irys_types::{BlockIndexItem, BlockIndexQuery, LedgerIndexItem, H256};
use serde::Serialize;

/// Maximum number of blocks that can be requested in a single query.
///
/// A hard limit protects the API from requests that would otherwise
/// require the server to iterate over a very large range of blocks,
/// potentially leading to excessive memory usage or denial of service.
const MAX_BLOCK_INDEX_QUERY_LIMIT: usize = 1_000;
const DEFAULT_BLOCK_INDEX_QUERY_LIMIT: usize = 100;

/// API response wrapper that preserves the `num_ledgers` field for
/// backward compatibility with external clients.
#[derive(Serialize)]
pub(crate) struct BlockIndexItemResponse {
    block_hash: H256,
    num_ledgers: u8,
    ledgers: Vec<LedgerIndexItem>,
}

impl From<BlockIndexItem> for BlockIndexItemResponse {
    fn from(item: BlockIndexItem) -> Self {
        Self {
            block_hash: item.block_hash,
            num_ledgers: u8::try_from(item.ledgers.len()).unwrap_or(u8::MAX),
            ledgers: item.ledgers,
        }
    }
}

pub(crate) async fn block_index_route(
    state: web::Data<ApiState>,
    query: web::Query<BlockIndexQuery>,
) -> Result<Json<Vec<BlockIndexItemResponse>>, ApiError> {
    let limit = if query.limit == 0 {
        DEFAULT_BLOCK_INDEX_QUERY_LIMIT
    } else {
        query.limit
    };
    if limit > MAX_BLOCK_INDEX_QUERY_LIMIT {
        return Err(ApiError::Custom(format!(
            "limit exceeds maximum allowed value of {MAX_BLOCK_INDEX_QUERY_LIMIT}"
        )));
    }
    let height = query.height;

    // Clone only the requested range while holding the read lock briefly
    let requested_blocks = state.block_index.read().get_range(height as u64, limit);

    Ok(Json(requested_blocks.into_iter().map(Into::into).collect()))
}
