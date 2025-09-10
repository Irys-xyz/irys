use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::{BlockIndexItem, BlockIndexQuery};

/// Maximum number of blocks that can be requested in a single query.
///
/// A hard limit protects the API from requests that would otherwise
/// require the server to iterate over a very large range of blocks,
/// potentially leading to excessive memory usage or denial of service.
const MAX_BLOCK_INDEX_QUERY_LIMIT: usize = 1_000;
const DEFAULT_BLOCK_INDEX_QUERY_LIMIT: usize = 100;

pub async fn block_index_route(
    state: web::Data<ApiState>,
    query: web::Query<BlockIndexQuery>,
) -> HttpResponse {
    let limit = if query.limit == 0 {
        DEFAULT_BLOCK_INDEX_QUERY_LIMIT
    } else {
        query.limit
    };
    if limit > MAX_BLOCK_INDEX_QUERY_LIMIT {
        return HttpResponse::BadRequest().body(format!(
            "limit exceeds maximum allowed value of {}",
            MAX_BLOCK_INDEX_QUERY_LIMIT
        ));
    }
    let height = query.height;

    // Clone only the requested range while holding the read lock briefly
    let requested_blocks: Vec<BlockIndexItem> = {
        let block_index_read = state.block_index.read();
        let total = block_index_read.items.len();
        let start = height.min(total);
        let end = start.saturating_add(limit).min(total);
        block_index_read.items[start..end].to_vec()
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&requested_blocks).unwrap())
}
