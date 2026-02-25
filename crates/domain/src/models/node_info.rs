use crate::{
    BlockIndexReadGuard, BlockTreeReadGuard, PeerList, chain_sync_state::ChainSyncState,
    get_canonical_chain,
};
use irys_types::{H256, NodeInfo};

pub async fn get_node_info(
    block_index: &BlockIndexReadGuard,
    block_tree: &BlockTreeReadGuard,
    peer_list: &PeerList,
    sync_state: &ChainSyncState,
    started_at: std::time::Instant,
    mining_address: irys_types::IrysAddress,
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
    let max_diff = block_tree.read().get_max_cumulative_difficulty_block();

    NodeInfo {
        version: "0.0.1".into(),
        peer_count: peer_list.peer_count(),
        chain_id,
        height: latest.height(),
        block_hash: latest.block_hash(),
        block_index_height,
        block_index_hash,
        pending_blocks: blocks as u64,
        is_syncing: sync_state.is_syncing(),
        current_sync_height: sync_state.sync_target_height(),
        uptime_secs: started_at.elapsed().as_secs(),
        cumulative_difficulty: max_diff.0,
        mining_address,
    }
}
