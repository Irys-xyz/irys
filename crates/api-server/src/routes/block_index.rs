use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::{BlockIndexItem, BlockIndexQuery};

/// Maximum number of blocks that can be requested in a single query.
///
/// A hard limit protects the API from requests that would otherwise
/// require the server to iterate over a very large range of blocks,
/// potentially leading to excessive memory usage or denial of service.
const MAX_BLOCK_INDEX_QUERY_LIMIT: usize = 1_000;

pub async fn block_index_route(
    state: web::Data<ApiState>,
    query: web::Query<BlockIndexQuery>,
) -> HttpResponse {
    let limit = query.limit;
    if limit > MAX_BLOCK_INDEX_QUERY_LIMIT {
        return HttpResponse::BadRequest().body(format!(
            "limit exceeds maximum allowed value of {}",
            MAX_BLOCK_INDEX_QUERY_LIMIT
        ));
    }
    let height = query.height;

    let block_index_read = state.block_index.read();
    let requested_blocks: Vec<&BlockIndexItem> = block_index_read
        .items
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= height && *i < height + limit)
        .map(|(_, block)| block)
        .collect();

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&requested_blocks).unwrap())
}
