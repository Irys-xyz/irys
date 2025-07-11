use irys_actors::block_tree_service::BlockTreeReadGuard;
use irys_domain::BlockIndexReadGuard;
use irys_types::block_provider::BlockProvider;
use irys_types::{BlockHash, BlockIndexItem, VDFLimiterInfo, H256};
use tracing::debug;
#[cfg(test)]
use {
    irys_actors::block_tree_service::BlockTreeCache,
    irys_database::BlockIndex,
    irys_types::{IrysBlockHeader, NodeConfig},
    std::sync::{Arc, RwLock},
    tracing::warn,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    /// The block is not in the index or tree.
    NotProcessed,
    /// The block is still in the tree. It might or might not
    /// be in the block index.
    ProcessedButCanBeReorganized,
    /// The block is in the index, but the tree has already pruned it.
    Finalized,
}

impl BlockStatus {
    pub fn is_processed(&self) -> bool {
        matches!(self, Self::Finalized | Self::ProcessedButCanBeReorganized)
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

    fn is_block_in_the_tree(&self, block_hash: &H256) -> bool {
        self.block_tree_read_guard
            .read()
            .get_block(block_hash)
            .is_some()
    }

    fn height_is_in_the_tree(&self, block_height: u64) -> bool {
        let binding = self.block_tree_read_guard.read();
        binding.get_hashes_for_height(block_height).is_some()
    }

    /// Returns the status of a block based on its height and hash.
    /// Possible statuses:
    /// - `NotProcessed`: The block is not in the index or tree.
    /// - `ProcessedButCanBeReorganized`: The block is still in the tree. It might or might not
    ///   be in the block index.
    /// - `Finalized`: The block is in the index, but the tree has already pruned it.
    pub fn block_status(&self, block_height: u64, block_hash: &BlockHash) -> BlockStatus {
        let block_is_in_the_tree = self.is_block_in_the_tree(block_hash);
        let height_is_in_the_tree = self.height_is_in_the_tree(block_height);
        let binding = self.block_index_read_guard.read();
        let index_item = binding.get_item(block_height);
        let height_is_in_the_index = index_item.is_some();

        let height_is_in_the_tree_but_the_block_is_not_processed =
            height_is_in_the_tree && !block_is_in_the_tree;

        if height_is_in_the_index {
            if block_is_in_the_tree {
                // Block has been processed, but all blocks in the tree are not considered finalized
                BlockStatus::ProcessedButCanBeReorganized
            } else if height_is_in_the_tree_but_the_block_is_not_processed {
                // Block might be a fork after a network partition
                BlockStatus::NotProcessed
            } else {
                // Block is in the index, but the tree has already pruned it
                BlockStatus::Finalized
            }
        } else if block_is_in_the_tree {
            // All blocks in the tree are a subject of reorganization
            BlockStatus::ProcessedButCanBeReorganized
        } else {
            // No information about the block in the index or tree
            BlockStatus::NotProcessed
        }
    }

    pub async fn wait_for_block_to_appear_in_index(&self, block_height: u64) {
        const ATTEMPTS_PER_SECOND: u64 = 5;

        loop {
            {
                let binding = self.block_index_read_guard.read();
                let index_item = binding.get_item(block_height);
                if index_item.is_some() {
                    return;
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(
                1000 / ATTEMPTS_PER_SECOND,
            ))
            .await;
        }
    }

    pub async fn wait_for_block_tree_to_catch_up(&self, block_height: u64) {
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
}

/// Testing utilities for `BlockStatusProvider` to simulate different tree/index states.
#[cfg(test)]
impl BlockStatusProvider {
    #[cfg(test)]
    pub async fn mock(node_config: &NodeConfig) -> Self {
        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(
                BlockTreeCache::new(
                    &IrysBlockHeader::new_mock_header(),
                    node_config.consensus_config(),
                ),
            ))),
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
    ) -> Vec<IrysBlockHeader> {
        let first_block = starting_block
            .map(|parent| IrysBlockHeader {
                block_hash: BlockHash::random(),
                height: parent.height + 1,
                previous_block_hash: parent.block_hash,
                ..IrysBlockHeader::new_mock_header()
            })
            .unwrap_or_else(|| IrysBlockHeader {
                block_hash: BlockHash::random(),
                height: 1,
                ..IrysBlockHeader::new_mock_header()
            });

        let mut blocks = vec![first_block];

        for _ in 1..num_blocks {
            let prev_block = blocks.last().expect("to have at least one block");
            let block = IrysBlockHeader {
                block_hash: BlockHash::random(),
                height: prev_block.height + 1,
                previous_block_hash: prev_block.block_hash,
                ..IrysBlockHeader::new_mock_header()
            };
            blocks.push(block);
        }

        blocks
    }

    #[cfg(test)]
    pub fn add_block_to_index_and_tree_for_testing(&self, block: &IrysBlockHeader) {
        let mut binding = self.block_index_read_guard.write();

        if binding.items.is_empty() {
            let genesis = IrysBlockHeader::default();
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
    pub fn add_block_mock_to_the_tree(&self, block: &IrysBlockHeader) {
        use irys_actors::block_tree_service::ema_snapshot::EmaSnapshot;
        use irys_database::CommitmentSnapshot;
        use irys_domain::EpochSnapshot;

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
    pub fn delete_mocked_blocks_older_than(&self, height: u64) {
        let mut latest_block = self.tree_tip();
        debug!("The tip is: {:?}", latest_block);
        let mut blocks_to_delete = vec![];

        while let Some(block) = self.get_block_from_tree(&latest_block) {
            if block.height < height {
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
