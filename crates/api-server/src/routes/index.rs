use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse,
};
use irys_domain::get_canonical_chain;
use irys_p2p::PeerList as _;
use irys_types::NodeInfo;

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    let block_index_height = state.block_index.read().latest_height();

    let (chain, blocks) = get_canonical_chain(state.block_tree.clone()).await.unwrap();
    let latest = chain.last().unwrap();

    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: state.peer_list.peer_count().await.unwrap_or(0),
        chain_id: state.config.consensus.chain_id,
        height: latest.height,
        block_hash: latest.block_hash,
        block_index_height,
        blocks: blocks as u64,
        is_syncing: state.sync_state.is_syncing(),
        current_sync_height: state.sync_state.sync_target_height(),
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&node_info).unwrap())
}
