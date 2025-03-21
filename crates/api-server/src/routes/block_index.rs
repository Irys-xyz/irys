use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_database::BlockIndexItem;

pub async fn block_index_route(state: web::Data<ApiState>) -> HttpResponse {
    let block_index = state.block_index.clone().expect("block index");

    let limit = 50;
    let height = 5;

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
