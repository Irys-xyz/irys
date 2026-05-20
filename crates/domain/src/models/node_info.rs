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
) -> eyre::Result<NodeInfo> {
    let (block_index_height, block_index_hash) = {
        let state = block_index.read();
        (
            state.latest_height(),
            state
                .get_latest_item()
                .map_or(H256::zero(), |i| i.block_hash),
        )
    };

    let chain = get_canonical_chain(block_tree.clone()).await?;
    let latest = chain
        .entries
        .last()
        .ok_or_else(|| eyre::eyre!("canonical chain is empty"))?;
    let max_diff = block_tree.read().get_max_cumulative_difficulty_block();

    Ok(NodeInfo {
        version: "0.0.1".into(),
        peer_count: u64::try_from(peer_list.peer_count())?,
        chain_id,
        height: latest.height(),
        block_hash: latest.block_hash(),
        block_index_height,
        block_index_hash,
        pending_blocks: u64::try_from(chain.not_onchain_count)?,
        is_syncing: sync_state.is_syncing(),
        current_sync_height: u64::try_from(sync_state.sync_target_height())?,
        uptime_secs: started_at.elapsed().as_secs(),
        cumulative_difficulty: max_diff.cumulative_diff,
        mining_address,
    })
}
