use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse,
};
use irys_types::H256;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct NodeInfo {
    pub version: String,
    pub peer_count: u32,
    pub chain_id: u64,
    pub height: u64,
    pub block_hash: H256,
    pub block_index_height: u64,
    pub blocks: u64,
}

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    let block_index_height = state
        .block_index
        .as_ref()
        .expect("block index")
        .read()
        .latest_height();
    // TODO: populate this response struct with real values
    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: 0,
        chain_id: state.config.chain_id,
        height: 0,
        block_hash: H256::zero(),
        block_index_height,
        blocks: 0,
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&node_info).unwrap())
}
