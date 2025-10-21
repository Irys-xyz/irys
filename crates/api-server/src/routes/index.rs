use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse,
};
use irys_domain::get_canonical_chain;
use irys_types::{Address, NodeInfo, H256};

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    let (block_index_height, block_index_hash) = {
        let state = state.block_index.read();
        (
            state.latest_height(),
            state
                .get_latest_item()
                .map_or(H256::zero(), |i| i.block_hash),
        )
    };

    let (chain, blocks) = get_canonical_chain(state.block_tree.clone()).await.unwrap();
    let latest = chain.last().unwrap();

    let node_info = NodeInfo {
        version: "0.0.1".into(),
        peer_count: state.peer_list.peer_count(),
        chain_id: state.config.consensus.chain_id,
        height: latest.height,
        block_hash: latest.block_hash,
        block_index_height,
        block_index_hash,
        pending_blocks: blocks as u64,
        is_syncing: state.sync_state.is_syncing(),
        current_sync_height: state.sync_state.sync_target_height(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        mining_address: Address::ZERO,
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .body(serde_json::to_string_pretty(&node_info).unwrap())
}

pub async fn genesis_route(state: web::Data<ApiState>) -> HttpResponse {
    let genesis_hash = state
        .block_index
        .read()
        .get_item(0)
        .map(|item| item.block_hash);

    if let Some(hash) = genesis_hash {
        let genesis_info = serde_json::json!({
            "genesis_block_hash": hash,
            "height": 0
        });

        HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(serde_json::to_string_pretty(&genesis_info).unwrap())
    } else {
        HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Genesis block not found in block index"
        }))
    }
}
