use crate::ApiState;
use actix_web::{
    web::{self},
    HttpResponse,
};
use irys_types::H256;
use serde::Serialize;


#[derive(Debug, Default, Serialize)]
struct NodeInfo {
    pub version: String,
    pub peer_count: u32,
    pub chain_id: u64,
    pub height: u64,
    pub block_hash: H256,
    pub blocks: u64,
}

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    // TODO: populate this response struct with real values
    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: 0,
        chain_id: state.config.chain_id,
        height: 0,
        block_hash: H256::zero(),
        blocks: 0,
    };
    HttpResponse::Ok().body(serde_json::to_string_pretty(&node_info).unwrap())
}
