use std::sync::{Arc, RwLock};
use irys_actors::block_index_service::BlockIndexReadGuard;
use irys_actors::block_tree_service::{BlockTreeCache, BlockTreeReadGuard};
use irys_database::BlockIndex;
use irys_types::{IrysBlockHeader, H256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    NotProcessed,
    ProcessedButSubjectToReorg,
    Processed,
}

pub trait BlockStatusProvider: Default + Clone + Unpin + Send + Sync + 'static {
    fn block_status_by_height(&self, height: u64) -> BlockStatus;
    fn block_status_by_hash(&self, hash: H256) -> BlockStatus;

    fn block_has_been_processed(&self, hash: H256) -> bool {
        match self.block_status_by_hash(hash) {
            BlockStatus::Processed => true,
            BlockStatus::ProcessedButSubjectToReorg => true,
            BlockStatus::NotProcessed => false,
        }
    }
}

#[derive(Clone)]
pub struct BlockStatusProviderImpl {
    block_index_read_guard: BlockIndexReadGuard,
    block_tree_read_guard: BlockTreeReadGuard
}

impl Default for BlockStatusProviderImpl {
    fn default() -> Self {
        Self {
            block_tree_read_guard: BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTreeCache::new(&IrysBlockHeader::default())))),
            block_index_read_guard: BlockIndexReadGuard::new(Arc::new(RwLock::new(BlockIndex::default())))
        }
    }
}

impl BlockStatusProvider for BlockStatusProviderImpl {
    fn block_status_by_height(&self, height: u64) -> BlockStatus {
        todo!()
    }

    fn block_status_by_hash(&self, hash: H256) -> BlockStatus {
        todo!()
    }
}


