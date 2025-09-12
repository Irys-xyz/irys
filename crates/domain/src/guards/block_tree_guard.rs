use crate::BlockTree;
use irys_types::LedgerChunkOffset;
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// Wraps the internal `Arc<RwLock<_>>` to make the reference readonly
#[derive(Debug, Clone)]
pub struct BlockTreeReadGuard {
    block_tree_cache: Arc<RwLock<BlockTree>>,
}

impl BlockTreeReadGuard {
    /// Creates a new `ReadGuard` for the `block_tree` cache
    pub const fn new(block_tree_cache: Arc<RwLock<BlockTree>>) -> Self {
        Self { block_tree_cache }
    }

    /// Accessor method to get a read guard for the `block_tree` cache
    pub fn read(&self) -> RwLockReadGuard<'_, BlockTree> {
        self.block_tree_cache.read().unwrap()
    }

    #[cfg(any(test, feature = "test-utils"))]
    /// Accessor method to get a write guard for the `block_tree` cache
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, BlockTree> {
        self.block_tree_cache.write().unwrap()
    }

    /// Gets the total number of chunks in a ledger at a given block height
    pub fn get_total_chunks(&self, block_height: u64, ledger_id: u32) -> Option<LedgerChunkOffset> {
        let tree = self.read();
        let (canonical, _) = tree.get_canonical_chain();

        let block_depth: usize = (canonical.last().unwrap().height - block_height) as usize;

        if canonical.len() >= block_depth {
            let block_entry = &canonical[canonical.len() - block_depth];

            let block = tree
                .get_block(&block_entry.block_hash)
                .expect("Block to be in block tree");

            let data_ledger = block
                .data_ledgers
                .iter()
                .find(|dl| dl.ledger_id == ledger_id)
                .expect("should be able to look up data_ledger by id");
            Some(data_ledger.total_chunks.into())
        } else {
            None
        }
    }
}
