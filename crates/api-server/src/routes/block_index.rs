use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_database::BlockIndexItem;

#[derive(serde::Deserialize)]
pub struct BlockIndexApiPath {
    height: usize,
    limit: usize,
}

pub async fn block_index_route(
    state: web::Data<ApiState>,
    path: web::Path<BlockIndexApiPath>,
) -> HttpResponse {
    let block_index = state.block_index.clone().expect("block index");

    let limit = path.limit;
    let height = path.height;

    let read = block_index.read();
    let requested_blocks: Vec<&BlockIndexItem> = read
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
