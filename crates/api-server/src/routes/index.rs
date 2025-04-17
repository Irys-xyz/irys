use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse,
};
use irys_actors::block_tree_service::get_canonical_chain;
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
    let block_index_height = state.block_index.read().latest_height();

    let (chain, blocks) = get_canonical_chain(state.block_tree.clone()).await.unwrap();
    let (block_hash, height, _, _) = chain.last().unwrap();

    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: 0,
        chain_id: state.config.consensus.chain_id,
        height: *height,
        block_hash: *block_hash,
        block_index_height,
        blocks: blocks as u64,
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&node_info).unwrap())
}
