use crate::BlockTree;
use irys_types::LedgerChunkOffset;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use tracing::{debug, error};

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

    /// Returns the height of the latest block on the canonical chain.
    pub fn latest_canonical_block_height(&self) -> Option<u64> {
        let tree = self.read();
        let (canonical, _) = tree.get_canonical_chain();
        canonical
            .last()
            .map(super::super::models::block_tree::BlockTreeEntry::height)
    }

    /// Gets the total number of chunks in a ledger at a given block height
    pub fn get_total_chunks(&self, block_height: u64, ledger_id: u32) -> Option<LedgerChunkOffset> {
        let tree = self.read();
        let (canonical, _) = tree.get_canonical_chain();

        let depth = canonical
            .last()
            .unwrap()
            .height()
            .saturating_sub(block_height) as usize;

        if canonical.len() > depth {
            let idx = canonical.len() - 1 - depth;
            let block_entry = &canonical[idx];

            let block = tree
                .get_block(&block_entry.block_hash())
                .expect("Block to be in block tree");

            match block
                .data_ledgers
                .iter()
                .find(|dl| dl.ledger_id == ledger_id)
            {
                Some(data_ledger) => Some(data_ledger.total_chunks.into()),
                // A pre-activation block legitimately predates a term ledger
                // (e.g. a pre-Cascade block for OneYear/ThirtyDay) and carries
                // no entry for it — report "no chunks for this ledger yet".
                None if tree
                    .consensus_config()
                    .hardforks
                    .ledger_absence_expected(ledger_id, block.timestamp_secs()) =>
                {
                    debug!(
                        ledger_id,
                        block_height = block.height,
                        "ledger not present at this height (pre-activation); reporting no chunks"
                    );
                    None
                }
                // Consensus requires a block to carry the full ledger set for
                // its activation state, so a missing entry here is an invariant
                // violation. Surface it loudly instead of masking it — but still
                // return None rather than aborting the process.
                None => {
                    error!(
                        ledger_id,
                        block_height = block.height,
                        "data ledger missing from a block where consensus requires it; reporting no chunks"
                    );
                    None
                }
            }
        } else {
            None
        }
    }
}
