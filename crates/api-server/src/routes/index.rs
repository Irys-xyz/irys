use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse,
};
use irys_domain::{
    chain_sync_state::ChainSyncState, get_canonical_chain, BlockIndexReadGuard, BlockTreeReadGuard,
    PeerList,
};
use irys_types::{NodeInfo, H256};

pub async fn get_node_info(
    block_index: &BlockIndexReadGuard,
    block_tree: &BlockTreeReadGuard,
    peer_list: &PeerList,
    sync_state: &ChainSyncState,
    started_at: std::time::Instant,
    mining_address: irys_types::Address,
    chain_id: u64,
) -> NodeInfo {
    let (block_index_height, block_index_hash) = {
        let state = block_index.read();
        (
            state.latest_height(),
            state
                .get_latest_item()
                .map_or(H256::zero(), |i| i.block_hash),
        )
    };

    let (chain, blocks) = get_canonical_chain(block_tree.clone()).await.unwrap();
    let latest = chain.last().unwrap();

    NodeInfo {
        version: "0.0.1".into(),
        peer_count: peer_list.peer_count(),
        chain_id,
        height: latest.height,
        block_hash: latest.block_hash,
        block_index_height,
        block_index_hash,
        pending_blocks: blocks as u64,
        is_syncing: sync_state.is_syncing(),
        current_sync_height: sync_state.sync_target_height(),
        uptime_secs: started_at.elapsed().as_secs(),
        mining_address,
    }
}

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    let node_info = get_node_info(
        &state.block_index,
        &state.block_tree,
        &state.peer_list,
        &state.sync_state,
        state.started_at,
        state.mining_address,
        state.config.consensus.chain_id,
    )
    .await;
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
