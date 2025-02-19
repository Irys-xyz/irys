use actix_web::HttpResponse;
use irys_types::{CONFIG, H256};
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

pub async fn info_route() -> HttpResponse {
    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: 0,
        chain_id: CONFIG.irys_chain_id,
        height: 0,
        block_hash: H256::zero(),
        blocks: 0,
    };
    HttpResponse::Ok().body(serde_json::to_string_pretty(&node_info).unwrap())
}
