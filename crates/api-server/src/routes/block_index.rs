use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_database::BlockIndexItem;

#[derive(serde::Deserialize)]
pub struct BlockIndexQuery {
    height: usize,
    limit: usize,
}

pub async fn block_index_route(
    state: web::Data<ApiState>,
    query: web::Query<BlockIndexQuery>,
) -> HttpResponse {
    let limit = query.limit;
    let height = query.height;

    let block_index_read = state.block_index.read();
    let requested_blocks: Vec<&BlockIndexItem> = block_index_read
        .items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i >= height && *i < height + limit)
        .map(|(_, block)| block)
        .collect();

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&requested_blocks).unwrap())
}
