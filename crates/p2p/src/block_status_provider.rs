use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard};
use irys_types::{block_provider::BlockProvider, BlockHash, BlockIndexItem, VDFLimiterInfo, H256};
use tracing::debug;
#[cfg(test)]
use {
    irys_types::{IrysBlockHeaderV1, NodeConfig, VersionedIrysBlockHeader},
    std::sync::{Arc, RwLock},
    tracing::warn,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
// TODO: expand these enum variants to account for all the actual states
pub enum BlockStatus {
    /// The block is not in the index or tree.
    NotProcessed,
    /// The block is still in the tree. It might or might not
    /// be in the block index.
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
    /// - `ProcessedButCanBeReorganized`: The block is still in the tree. It might or might not
    ///   be in the block index.
    /// - `Finalized`: The block is in the index, but the tree has already pruned it.
    /// TODO: this needs to handle migrated block reorgs, it does not currently :)
    /// NOTE: block height is UNTRUSTED here, we haven't validated it yet
    pub fn block_status(&self, block_height: u64, block_hash: &BlockHash) -> BlockStatus {
        let block_is_anywhere_in_the_tree = self.is_block_in_the_tree(block_hash);
        let binding = self.block_index_read_guard.read();
        let index_item = binding.get_item(block_height);
        let hash_is_in_the_index = index_item.is_some_and(|idx| idx.block_hash == *block_hash);
        let height_is_in_the_index = index_item.is_some();

        if height_is_in_the_index {
            if hash_is_in_the_index {
                // Block is in the block index, it has been migrated
                return BlockStatus::Finalized;
            } else {
                return BlockStatus::PartOfAPrunedFork;
            }
        }

        if !height_is_in_the_index && block_is_anywhere_in_the_tree {
            // All blocks in the tree are a subject of reorganization
            BlockStatus::ProcessedButCanBeReorganized
        } else
        /* !height_is_in_the_index && !block_is_anywhere_in_the_tree */
        {
            // No information about the block in the index or tree
            BlockStatus::NotProcessed
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
        binding.get_latest_item().cloned()
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
        binding.get_latest_canonical_entry().height
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
    pub async fn mock(node_config: &NodeConfig) -> Self {
        use irys_domain::{BlockIndex, BlockTree};

        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
                &VersionedIrysBlockHeader::new_mock_header(),
                node_config.consensus_config(),
            )))),
            block_index_read_guard: BlockIndexReadGuard::new(Arc::new(RwLock::new(
                BlockIndex::new(node_config)
                    .await
                    .expect("to create a mock block index"),
            ))),
        }
    }

    #[cfg(test)]
    pub fn tree_tip(&self) -> BlockHash {
        self.block_tree_read_guard.read().tip
    }

    #[cfg(test)]
    pub fn get_block_from_tree(&self, block_hash: &BlockHash) -> Option<VersionedIrysBlockHeader> {
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
        starting_block: Option<&VersionedIrysBlockHeader>,
    ) -> Vec<VersionedIrysBlockHeader> {
        let first_block = starting_block
            .map(|parent| {
                VersionedIrysBlockHeader::V1(IrysBlockHeaderV1 {
                    block_hash: BlockHash::random(),
                    height: parent.height + 1,
                    previous_block_hash: parent.block_hash,
                    ..IrysBlockHeaderV1::new_mock_header()
                })
            })
            .unwrap_or_else(|| {
                VersionedIrysBlockHeader::V1(IrysBlockHeaderV1 {
                    block_hash: BlockHash::random(),
                    height: 1,
                    ..IrysBlockHeaderV1::new_mock_header()
                })
            });

        let mut blocks = vec![first_block];

        for _ in 1..num_blocks {
            let prev_block = blocks.last().expect("to have at least one block");
            let block = VersionedIrysBlockHeader::V1(IrysBlockHeaderV1 {
                block_hash: BlockHash::random(),
                height: prev_block.height + 1,
                previous_block_hash: prev_block.block_hash,
                ..IrysBlockHeaderV1::new_mock_header()
            });
            blocks.push(block);
        }

        blocks
    }

    #[cfg(test)]
    pub fn add_block_to_index_and_tree_for_testing(&self, block: &VersionedIrysBlockHeader) {
        let mut binding = self.block_index_read_guard.write();

        if binding.items.is_empty() {
            let genesis = VersionedIrysBlockHeader::default();
            binding
                .push_item(&BlockIndexItem {
                    block_hash: genesis.block_hash,
                    num_ledgers: 0,
                    ledgers: vec![],
                })
                .unwrap();
        }

        binding
            .push_item(&BlockIndexItem {
                block_hash: block.block_hash,
                num_ledgers: 0,
                ledgers: vec![],
            })
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
    pub fn add_block_mock_to_the_tree(&self, block: &VersionedIrysBlockHeader) {
        use irys_domain::{CommitmentSnapshot, EmaSnapshot, EpochSnapshot};

        self.block_tree_read_guard
            .write()
            .add_block(
                block,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                Arc::new(EmaSnapshot::default()),
            )
            .expect("to add block to the tree");
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

        let latest_canonical_hash = binding.get_latest_canonical_entry().block_hash;
        binding
            .get_block(&latest_canonical_hash)
            .map(|block| block.vdf_limiter_info.clone())
    }
}
