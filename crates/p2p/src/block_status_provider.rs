use irys_actors::block_index_service::BlockIndexReadGuard;
use irys_actors::block_tree_service::BlockTreeReadGuard;
use irys_types::{BlockHash, H256};
#[cfg(test)]
use {
    irys_actors::block_tree_service::BlockTreeCache,
    irys_database::BlockIndex,
    irys_types::{BlockIndexItem, IrysBlockHeader, NodeConfig},
    std::sync::{Arc, RwLock},
    tracing::warn,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MismatchedBlockHashes {
    pub hash_in_index: BlockHash,
    pub hash_in_tree: BlockHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    NotProcessed,
    ProcessedButSubjectToReorg,
    Processed,
    IndexHashMismatch(MismatchedBlockHashes),
}

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

    pub fn block_is_in_the_tree(&self, block_hash: &H256) -> bool {
        self.block_tree_read_guard
            .read()
            .get_block(block_hash)
            .is_some()
    }

    /// If the block height is unknown
    pub fn block_status_from_tree(&self, hash: &BlockHash) -> BlockStatus {
        let binding = self.block_tree_read_guard.read();
        let index_item = binding.get_block(hash);

        if let Some(index_item) = index_item {
            let block_index_binding = self.block_index_read_guard.read();
            let index_item = block_index_binding.get_item(index_item.height);

            if let Some(index_item) = index_item {
                if &index_item.block_hash == hash {
                    BlockStatus::Processed
                } else {
                    BlockStatus::IndexHashMismatch(MismatchedBlockHashes {
                        hash_in_index: index_item.block_hash,
                        hash_in_tree: *hash,
                    })
                }
            } else {
                BlockStatus::ProcessedButSubjectToReorg
            }
        } else {
            BlockStatus::NotProcessed
        }
    }

    pub fn block_status(&self, block_height: u64, block_hash: &BlockHash) -> BlockStatus {
        let block_is_in_the_tree = self.block_is_in_the_tree(block_hash);
        let binding = self.block_index_read_guard.read();
        let index_item = binding.get_item(block_height);

        println!("{:?}", binding.items);

        if let Some(index_item) = index_item {
            if &index_item.block_hash == block_hash {
                BlockStatus::Processed
            } else {
                BlockStatus::IndexHashMismatch(MismatchedBlockHashes {
                    hash_in_index: index_item.block_hash,
                    hash_in_tree: *block_hash,
                })
            }
        } else if block_is_in_the_tree {
            BlockStatus::ProcessedButSubjectToReorg
        } else {
            BlockStatus::NotProcessed
        }
    }

    pub fn block_has_been_processed_by_the_tree(&self, hash: &BlockHash) -> bool {
        match self.block_status_from_tree(hash) {
            BlockStatus::Processed => true,
            BlockStatus::ProcessedButSubjectToReorg => true,
            BlockStatus::NotProcessed => false,
            BlockStatus::IndexHashMismatch(_) => false,
        }
    }
}

#[cfg(test)]
impl BlockStatusProvider {
    pub async fn mock(node_config: &NodeConfig) -> Self {
        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(
                BlockTreeCache::new(&IrysBlockHeader::new_mock_header()),
            ))),
            block_index_read_guard: BlockIndexReadGuard::new(Arc::new(RwLock::new(
                BlockIndex::new(node_config)
                    .await
                    .expect("to create a mock block index"),
            ))),
        }
    }

    pub fn add_block_to_index_and_tree(&self, block: IrysBlockHeader) {
        self.block_tree_read_guard
            .write()
            .add_block(&block, Arc::new(Vec::new()))
            .expect("to add block to the tree");
        self.block_index_read_guard
            .write()
            .push_item(&BlockIndexItem {
                block_hash: block.block_hash,
                num_ledgers: 0,
                ledgers: vec![],
            })
            .unwrap();
        warn!(
            "Added block {:?} (height {}) to index and tree",
            block.block_hash, block.height
        );
    }

    pub fn add_block_to_the_tree(&self, block: IrysBlockHeader) {
        self.block_tree_read_guard
            .write()
            .add_block(&block, Arc::new(Vec::new()))
            .expect("to add block to the tree");
    }
}
