use irys_actors::block_index_service::BlockIndexReadGuard;
use irys_actors::block_tree_service::{BlockTreeCache, BlockTreeReadGuard};
use irys_database::BlockIndex;
use irys_types::{BlockHash, IrysBlockHeader, H256};
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    NotProcessed,
    ProcessedButSubjectToReorg,
    Processed,
    IndexHashMismatch,
}

#[derive(Clone, Debug)]
pub struct BlockStatusProvider {
    block_index_read_guard: BlockIndexReadGuard,
    block_tree_read_guard: BlockTreeReadGuard,
}

impl Default for BlockStatusProvider {
    fn default() -> Self {
        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(
                BlockTreeCache::new(&IrysBlockHeader::default()),
            ))),
            block_index_read_guard: BlockIndexReadGuard::new(Arc::new(RwLock::new(
                BlockIndex::default(),
            ))),
        }
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
                    BlockStatus::IndexHashMismatch
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

        if let Some(index_item) = index_item {
            if &index_item.block_hash == block_hash {
                BlockStatus::Processed
            } else {
                BlockStatus::IndexHashMismatch
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
            BlockStatus::IndexHashMismatch => false,
        }
    }
}
