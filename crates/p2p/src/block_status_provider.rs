use irys_domain::{BlockIndexReadGuard, BlockState, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, BlockIndexItem, H256, VDFLimiterInfo, block_provider::BlockProvider};
use tracing::debug;
#[cfg(test)]
use {
    irys_testing_utils::IrysBlockHeaderTestExt as _,
    irys_types::{
        ConsensusConfig, IrysBlockHeader, IrysBlockHeaderV1, NodeConfig, irys::IrysSigner,
    },
    std::sync::{Arc, RwLock},
    tracing::warn,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    /// The block is not in the index or tree.
    NotProcessed,
    /// The block exists in the in-memory tree but its validation has not
    /// completed (`ChainState::NotOnchain(Unknown|ValidationScheduled)` or
    /// `Validated(Unknown|ValidationScheduled)`). Distinguished from
    /// `NotProcessed` so the orphan re-pull branch in
    /// `block_pool::process_block` recognises the parent as locally known
    /// and does not re-pull from the network; distinguished from
    /// `ProcessedButCanBeReorganized` so `is_processed()` still reflects
    /// that validation hasn't finished.
    InTreePendingValidation,
    /// The block is in the in-memory tree and has been validated (either
    /// canonical `Onchain` or a `Validated`/`NotOnchain(ValidBlock)` fork).
    ProcessedButCanBeReorganized,
    /// The block is in the index, but the tree has already pruned it.
    Finalized,
    /// The block is part of a fork that has been pruned from the main chain.
    PartOfAPrunedFork,
}

impl BlockStatus {
    pub fn is_processed(&self) -> bool {
        matches!(
            self,
            Self::Finalized | Self::ProcessedButCanBeReorganized | Self::PartOfAPrunedFork
        )
    }

    /// True iff the block is observable in the in-memory tree or has been
    /// finalized into the block index. Used to distinguish "parent genuinely
    /// missing" (orphan re-pull is appropriate) from "parent locally known"
    /// (no re-pull needed — the child can proceed through prevalidation).
    pub fn is_in_tree(&self) -> bool {
        matches!(
            self,
            Self::InTreePendingValidation | Self::ProcessedButCanBeReorganized | Self::Finalized
        )
    }

    pub fn is_a_part_of_pruned_fork(&self) -> bool {
        matches!(self, Self::PartOfAPrunedFork)
    }
}

/// Provides information about the status of a block in the context of the block tree and index.
#[derive(Clone, Debug)]
pub struct BlockStatusProvider {
    block_index_read_guard: BlockIndexReadGuard,
    block_tree_read_guard: BlockTreeReadGuard,
}

impl Default for BlockStatusProvider {
    fn default() -> Self {
        panic!("If you want to mock BlockStatusProvider, use `BlockStatusProvider::mock` instead.")
    }
}

impl BlockStatusProvider {
    pub fn new(
        block_index_read_guard: BlockIndexReadGuard,
        block_tree_read_guard: BlockTreeReadGuard,
    ) -> Self {
        Self {
            block_tree_read_guard,
            block_index_read_guard,
        }
    }

    pub fn is_block_in_the_tree(&self, block_hash: &H256) -> bool {
        self.block_tree_read_guard
            .read()
            .get_block(block_hash)
            .is_some()
    }

    /// Returns the status of a block based on its height and hash.
    /// Possible statuses:
    /// - `NotProcessed`: The block is not in the index or tree.
    /// - `InTreePendingValidation`: The block is in the tree but validation has not completed.
    /// - `ProcessedButCanBeReorganized`: The block is in the tree and has been validated; it
    ///   may or may not be in the block index.
    /// - `Finalized`: The block is in the index, but the tree has already pruned it.
    /// - `PartOfAPrunedFork`: A different block has been migrated at this height.
    ///
    /// TODO: this needs to handle migrated block reorgs, it does not currently :)
    /// NOTE: block height is UNTRUSTED here, we haven't validated it yet
    pub fn block_status(&self, block_height: u64, block_hash: &BlockHash) -> BlockStatus {
        let binding = self.block_index_read_guard.read();
        let index_item = binding.get_item(block_height);
        let height_is_in_the_index = index_item.is_some();
        let hash_is_in_the_index = index_item
            .as_ref()
            .is_some_and(|idx| idx.block_hash == *block_hash);

        if height_is_in_the_index {
            if hash_is_in_the_index {
                // Block is in the block index, it has been migrated
                return BlockStatus::Finalized;
            } else {
                return BlockStatus::PartOfAPrunedFork;
            }
        }

        // Index has nothing at this height. Distinguish "block missing from tree"
        // (orphan — re-pull from network) from "block in tree, validation pending"
        // (parent is locally known — children of such a parent must not trigger
        // the orphan re-pull path).
        match self
            .block_tree_read_guard
            .read()
            .get_block_and_status(block_hash)
            .map(|(_, state)| *state)
        {
            None => BlockStatus::NotProcessed,
            Some(
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled)
                | ChainState::Validated(BlockState::Unknown | BlockState::ValidationScheduled),
            ) => BlockStatus::InTreePendingValidation,
            Some(
                ChainState::Onchain
                | ChainState::Validated(BlockState::ValidBlock)
                | ChainState::NotOnchain(BlockState::ValidBlock),
            ) => BlockStatus::ProcessedButCanBeReorganized,
        }
    }

    pub async fn wait_for_block_to_appear_in_index(&self, block_height: u64) {
        const ATTEMPTS_PER_SECOND: u64 = 5;
        let mut attempts = 0;
        loop {
            {
                let binding = self.block_index_read_guard.read();
                let index_item = binding.get_item(block_height);
                if index_item.is_some() {
                    return;
                }
            }

            if attempts % ATTEMPTS_PER_SECOND == 0 {
                debug!(
                    "Waiting for block {} to appear in the block index...",
                    &block_height
                );
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(
                1000 / ATTEMPTS_PER_SECOND,
            ))
            .await;

            attempts += 1;
        }
    }

    // waits for the block tree to be "able" to processed this block, based on height.
    // this is to prevent overwhelming the block tree with blocks, such that it prunes blocks still undergoing validation.
    pub async fn wait_for_block_tree_can_process_height(&self, block_height: u64) {
        const ATTEMPTS_PER_SECOND: u64 = 5;
        let mut attempts = 0;

        loop {
            attempts += 1;

            if attempts % ATTEMPTS_PER_SECOND == 0 {
                debug!(
                    "Block tree did not catch up to height {} after {} seconds, waiting...",
                    block_height,
                    attempts / ATTEMPTS_PER_SECOND
                );
            }

            let can_process_height = {
                let binding = self.block_tree_read_guard.read();
                binding.can_process_height(block_height)
            };

            if can_process_height {
                return;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(
                1000 / ATTEMPTS_PER_SECOND,
            ))
            .await;
        }
    }

    pub fn is_height_in_the_index(&self, block_height: u64) -> bool {
        let binding = self.block_index_read_guard.read();
        let index_item = binding.get_item(block_height);
        index_item.is_some()
    }

    pub fn latest_block_in_index(&self) -> Option<BlockIndexItem> {
        let binding = self.block_index_read_guard.read();
        binding.get_latest_item()
    }

    /// Get the block tree read guard
    pub fn block_tree_read_guard(&self) -> &BlockTreeReadGuard {
        &self.block_tree_read_guard
    }

    /// Get the block index read guard
    pub fn block_index_read_guard(&self) -> &BlockIndexReadGuard {
        &self.block_index_read_guard
    }

    pub fn canonical_height(&self) -> u64 {
        let binding = self.block_tree_read_guard.read();
        binding.get_latest_canonical_entry().height()
    }

    pub fn index_height(&self) -> u64 {
        self.block_index_read_guard.read().latest_height()
    }

    pub fn block_index(&self) -> BlockIndexReadGuard {
        self.block_index_read_guard.clone()
    }
}

/// Testing utilities for `BlockStatusProvider` to simulate different tree/index states.
#[cfg(test)]
impl BlockStatusProvider {
    #[cfg(test)]
    pub fn mock(node_config: &NodeConfig, db: irys_types::DatabaseProvider) -> Self {
        use irys_domain::{BlockIndex, BlockTree};

        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = BlockHash::zero();
        genesis.test_sign();
        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
                &genesis,
                node_config.consensus_config(),
            )))),
            block_index_read_guard: BlockIndexReadGuard::new(BlockIndex::new_for_testing(db)),
        }
    }

    #[cfg(test)]
    pub fn tree_tip(&self) -> BlockHash {
        self.block_tree_read_guard.read().tip
    }

    /// Returns the genesis header from the mock tree (the tip of a freshly created mock).
    #[cfg(test)]
    pub fn genesis_header(&self) -> IrysBlockHeader {
        self.get_block_from_tree(&self.tree_tip())
            .expect("mock tree should have a genesis block")
    }

    #[cfg(test)]
    pub fn get_block_from_tree(&self, block_hash: &BlockHash) -> Option<IrysBlockHeader> {
        self.block_tree_read_guard
            .read()
            .get_block(block_hash)
            .cloned()
    }

    #[cfg(test)]
    pub fn oldest_tree_height(&self) -> u64 {
        let mut latest_block = self.tree_tip();
        let mut oldest_height = 0;
        debug!("The tip is: {:?}", latest_block);

        while let Some(block) = self.get_block_from_tree(&latest_block) {
            oldest_height = block.height;
            if block.previous_block_hash != BlockHash::zero() {
                latest_block = block.previous_block_hash;
            } else {
                break;
            }
        }

        debug!(
            "The oldest block height in the tree is: {} ({:?})",
            oldest_height, latest_block
        );
        oldest_height
    }

    #[cfg(test)]
    pub fn produce_mock_chain(
        num_blocks: u64,
        starting_block: Option<&IrysBlockHeader>,
        consensus_config: &ConsensusConfig,
    ) -> Vec<IrysBlockHeader> {
        let random_signer = IrysSigner::random_signer(consensus_config);
        let mut first_block = starting_block
            .map(|parent| {
                IrysBlockHeader::V1(IrysBlockHeaderV1 {
                    block_hash: BlockHash::random(),
                    height: parent.height + 1,
                    previous_block_hash: parent.block_hash,
                    ..IrysBlockHeaderV1::new_mock_header()
                })
            })
            .unwrap_or_else(|| {
                IrysBlockHeader::V1(IrysBlockHeaderV1 {
                    block_hash: BlockHash::random(),
                    height: 1,
                    ..IrysBlockHeaderV1::new_mock_header()
                })
            });
        random_signer
            .sign_block_header(&mut first_block)
            .expect("to sign block");

        let mut blocks = vec![first_block];

        for _ in 1..num_blocks {
            let prev_block = blocks.last().expect("to have at least one block");
            let mut block = IrysBlockHeader::V1(IrysBlockHeaderV1 {
                block_hash: BlockHash::random(),
                height: prev_block.height + 1,
                previous_block_hash: prev_block.block_hash,
                ..IrysBlockHeaderV1::new_mock_header()
            });
            random_signer
                .sign_block_header(&mut block)
                .expect("to sign block");
            blocks.push(block);
        }

        blocks
    }

    #[cfg(test)]
    pub fn add_block_to_index_and_tree_for_testing(&self, block: &IrysBlockHeader) {
        let block_index = self.block_index_read_guard.read();

        if block_index.num_blocks() == 0 {
            let genesis = IrysBlockHeader::default();
            block_index
                .push_item(
                    &BlockIndexItem {
                        block_hash: genesis.block_hash,
                        num_ledgers: 0,
                        ledgers: vec![],
                    },
                    0,
                )
                .unwrap();
        }

        let next_height = block_index.num_blocks();
        block_index
            .push_item(
                &BlockIndexItem {
                    block_hash: block.block_hash,
                    num_ledgers: 0,
                    ledgers: vec![],
                },
                next_height,
            )
            .unwrap();
        warn!(
            "Added block {:?} (height {}) to index",
            block.block_hash, block.height
        );

        self.add_block_mock_to_the_tree(block);
        warn!(
            "Added block {:?} (height {}) to index and tree",
            block.block_hash, block.height
        );
    }

    #[cfg(test)]
    pub fn add_block_mock_to_the_tree(&self, block: &IrysBlockHeader) {
        use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};
        use irys_types::{BlockTransactions, SealedBlock};

        let sealed = Arc::new(SealedBlock::new_unchecked(
            Arc::new(block.clone()),
            BlockTransactions::default(),
        ));

        self.block_tree_read_guard
            .write()
            .add_block(
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                Arc::new(EmaSnapshot::default()),
            )
            .expect("to add block to the tree");
    }

    /// Like `add_block_mock_to_the_tree` but inserts the block in an arbitrary
    /// `ChainState` via `add_common` directly — used to exercise the
    /// `Validated(Unknown|ValidationScheduled)` branches of `block_status`.
    #[cfg(test)]
    pub fn add_block_mock_with_state(&self, block: &IrysBlockHeader, chain_state: ChainState) {
        use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};
        use irys_types::{BlockTransactions, SealedBlock};

        let sealed = Arc::new(SealedBlock::new_unchecked(
            Arc::new(block.clone()),
            BlockTransactions::default(),
        ));

        self.block_tree_read_guard
            .write()
            .add_common(
                block.block_hash,
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                Arc::new(EmaSnapshot::default()),
                chain_state,
            )
            .expect("to add block to the tree with chain_state");
    }

    #[cfg(test)]
    pub fn set_tip_for_testing(&self, block_hash: &BlockHash) {
        self.block_tree_read_guard.write().tip = *block_hash;
        warn!("Marked block {:?} as tip", block_hash);
    }

    #[cfg(test)]
    pub fn delete_mocked_blocks_older_than(&self, cutoff: u64) {
        let mut latest_block = self.tree_tip();
        debug!("The tip is: {:?}", latest_block);
        let mut blocks_to_delete = vec![];

        while let Some(block) = self.get_block_from_tree(&latest_block) {
            if block.height < cutoff {
                blocks_to_delete.push(block.block_hash);
            }

            if block.previous_block_hash != BlockHash::zero() {
                latest_block = block.previous_block_hash;
            } else {
                debug!("No previous block hash found, breaking the loop.");
                break;
            }
        }

        for block_hash in blocks_to_delete {
            self.block_tree_read_guard
                .write()
                .test_delete(&block_hash)
                .expect("to delete block from the tree");
            debug!("Deleted block {:?} from the tree", block_hash);
        }
    }
}

impl BlockProvider for BlockStatusProvider {
    fn latest_canonical_vdf_info(&self) -> Option<VDFLimiterInfo> {
        let binding = self.block_tree_read_guard.read();

        let latest_canonical_hash = binding.get_latest_canonical_entry().block_hash();
        binding
            .get_block(&latest_canonical_hash)
            .map(|block| block.vdf_limiter_info.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::BlockState;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{DatabaseProvider, DbSyncMode, NodeConfig};
    use rstest::rstest;
    use std::sync::Arc;

    fn mock_provider() -> (BlockStatusProvider, IrysBlockHeader, tempfile::TempDir) {
        let tmp_dir = TempDirBuilder::new()
            .prefix("block-status-provider-test-")
            .build();
        let db_env =
            open_or_create_irys_consensus_data_db(tmp_dir.path(), DbSyncMode::UtterlyNoSync)
                .expect("to open consensus db");
        let db = DatabaseProvider(Arc::new(db_env));
        let node_config = NodeConfig::testing();
        let provider = BlockStatusProvider::mock(&node_config, db);
        let genesis = provider.genesis_header();
        (provider, genesis, tmp_dir)
    }

    // Every `ChainState` variant that should resolve to `InTreePendingValidation`:
    //
    // - `NotOnchain(Unknown)`: default state after `BlockTree::add_block` —
    //   the canonical "in tree pending validation" state.
    // - `Validated(Unknown)`: locally-produced blocks are inserted directly as
    //   `ChainState::Validated(BlockState::Unknown)` (see `BlockTree::add_common`
    //   call sites). `block_status` must still treat this as
    //   `InTreePendingValidation` so children of a not-yet-fully-validated local
    //   block don't enter the orphan re-pull path.
    // - `Validated(ValidationScheduled)`: the state a locally produced block
    //   transitions to once `mark_block_as_validation_scheduled` fires but
    //   before validation completes.
    #[rstest]
    #[case(ChainState::NotOnchain(BlockState::Unknown))]
    #[case(ChainState::Validated(BlockState::Unknown))]
    #[case(ChainState::Validated(BlockState::ValidationScheduled))]
    fn block_status_returns_in_tree_pending_validation(#[case] chain_state: ChainState) {
        let (provider, genesis, _tmp) = mock_provider();
        let consensus = NodeConfig::testing().consensus_config();
        let chain = BlockStatusProvider::produce_mock_chain(1, Some(&genesis), &consensus);
        let pending = &chain[0];

        provider.add_block_mock_with_state(pending, chain_state);

        let status = provider.block_status(pending.height, &pending.block_hash);
        assert_eq!(status, BlockStatus::InTreePendingValidation);
        assert!(status.is_in_tree());
        assert!(!status.is_processed());
    }

    #[test]
    fn block_status_returns_processed_for_validated_block() {
        let (provider, genesis, _tmp) = mock_provider();
        let consensus = NodeConfig::testing().consensus_config();
        let chain = BlockStatusProvider::produce_mock_chain(1, Some(&genesis), &consensus);
        let validated = &chain[0];

        provider.add_block_mock_to_the_tree(validated);
        // Mark validation scheduled, then valid.
        provider
            .block_tree_read_guard
            .write()
            .mark_block_as_validation_scheduled(&validated.block_hash)
            .expect("schedule validation");
        provider
            .block_tree_read_guard
            .write()
            .mark_block_as_valid(&validated.block_hash)
            .expect("mark valid");

        let status = provider.block_status(validated.height, &validated.block_hash);
        assert_eq!(status, BlockStatus::ProcessedButCanBeReorganized);
        assert!(status.is_in_tree());
        assert!(status.is_processed());
    }

    #[test]
    fn block_status_returns_not_processed_for_missing_block() {
        let (provider, _genesis, _tmp) = mock_provider();
        let unknown_hash = BlockHash::repeat_byte(0x42);
        let status = provider.block_status(7, &unknown_hash);
        assert_eq!(status, BlockStatus::NotProcessed);
        assert!(!status.is_in_tree());
        assert!(!status.is_processed());
    }
}
