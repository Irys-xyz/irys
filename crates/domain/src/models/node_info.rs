use crate::{
    BlockIndexReadGuard, BlockTreeReadGuard, PeerList, chain_sync_state::ChainSyncState,
    get_canonical_chain,
};
use irys_types::{DatabaseVersion, H256, NodeInfo, ProtocolVersion, git_dirty, git_sha};

pub async fn get_node_info(
    block_index: &BlockIndexReadGuard,
    block_tree: &BlockTreeReadGuard,
    peer_list: &PeerList,
    sync_state: &ChainSyncState,
    started_at: std::time::Instant,
    mining_address: irys_types::IrysAddress,
    chain_id: u64,
    consensus_config_hash: H256,
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

    let (chain, blocks) = get_canonical_chain(block_tree.clone()).await?;
    let latest = chain
        .last()
        .ok_or_else(|| eyre::eyre!("canonical chain is empty"))?;

    Ok(NodeInfo {
        version: irys_types::get_version().to_string(),
        peer_count: u64::try_from(peer_list.peer_count())?,
        chain_id,
        height: latest.height(),
        block_hash: latest.block_hash(),
        block_index_height,
        block_index_hash,
        pending_blocks: u64::try_from(blocks)?,
        is_syncing: sync_state.is_syncing(),
        current_sync_height: u64::try_from(sync_state.sync_target_height())?,
        uptime_secs: started_at.elapsed().as_secs(),
        // The tip's difficulty, so it matches `height` and `block_hash`. The block with
        // the max cumulative difficulty can still wait for validation.
        cumulative_difficulty: latest.header().cumulative_diff,
        mining_address,
        git_sha: git_sha(),
        dirty: git_dirty(),
        protocol_version: ProtocolVersion::current() as u32,
        supported_protocol_versions: ProtocolVersion::supported_versions_u32().to_vec(),
        db_schema_version: u32::from(DatabaseVersion::CURRENT),
        consensus_config_hash: Some(consensus_config_hash),
    })
}

#[cfg(test)]
mod tests {
    use super::get_node_info;
    use crate::{
        BlockIndex, BlockIndexReadGuard, BlockTree, BlockTreeReadGuard, CommitmentSnapshot,
        EmaSnapshot, EpochSnapshot, PeerList, chain_sync_state::ChainSyncState,
    };
    use irys_database::tables::IrysTables;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        BlockIndexItem, BlockTransactions, ConsensusConfig, DatabaseProvider, H256, IrysAddress,
        IrysBlockHeader, SealedBlock, U256,
    };
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::{Arc, RwLock};

    /// Node info reports the tip: a heavier block that still waits for validation does
    /// not change the advertised height, hash or cumulative difficulty.
    #[tokio::test]
    async fn node_info_reports_the_tip_not_a_pending_heavier_block() -> eyre::Result<()> {
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.cumulative_diff = U256::from(100);
        genesis.test_sign();
        let mut tree = BlockTree::new(&genesis, ConsensusConfig::testing());

        let mut pending = IrysBlockHeader::new_mock_header();
        pending.height = 1;
        pending.previous_block_hash = genesis.block_hash;
        pending.cumulative_diff = U256::from(200);
        pending.test_sign();
        let sealed = Arc::new(SealedBlock::new_unchecked(
            Arc::new(pending.clone()),
            BlockTransactions::default(),
        ));
        tree.add_block(
            &sealed,
            Arc::new(CommitmentSnapshot::default()),
            Arc::new(EpochSnapshot::default()),
            EmaSnapshot::genesis(sealed.header()),
        )?;
        assert_eq!(
            tree.get_max_cumulative_difficulty_block().1,
            pending.block_hash
        );

        let tmp = TempDirBuilder::new().prefix("node-info-tip").build();
        let db = DatabaseProvider(Arc::new(open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?));
        let index = BlockIndex::new_for_testing(db);
        index.push_item(
            &BlockIndexItem {
                block_hash: genesis.block_hash,
                num_ledgers: 0,
                ledgers: Vec::new(),
            },
            0,
        )?;

        let info = get_node_info(
            &BlockIndexReadGuard::new(index),
            &BlockTreeReadGuard::new(Arc::new(RwLock::new(tree))),
            &PeerList::test_mock()?,
            &ChainSyncState::new(false, false),
            std::time::Instant::now(),
            IrysAddress::random(),
            1,
            H256::zero(),
        )
        .await?;

        assert_eq!(info.height, 0);
        assert_eq!(info.block_hash, genesis.block_hash);
        assert_eq!(info.cumulative_difficulty, genesis.cumulative_diff);
        Ok(())
    }
}
