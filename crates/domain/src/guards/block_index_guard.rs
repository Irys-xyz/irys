use irys_database::block_header_by_hash;
use irys_types::{BlockIndexItem, DatabaseProvider};
use reth_db::Database as _;
use tracing::{debug, error};

use crate::BlockIndex;

/// Read-only wrapper around [`BlockIndex`].
///
/// Since `BlockIndex` is now backed by MDBX (which handles its own
/// concurrency), no `RwLock` is needed.
#[derive(Debug, Clone)]
pub struct BlockIndexReadGuard {
    block_index: BlockIndex,
}

impl BlockIndexReadGuard {
    /// Creates a new `BlockIndexReadGuard`
    pub fn new(block_index: BlockIndex) -> Self {
        Self { block_index }
    }

    /// Returns a reference to the inner `BlockIndex`
    pub fn read(&self) -> &BlockIndex {
        &self.block_index
    }

    /// Returns a clone of the inner `BlockIndex`
    pub fn inner(&self) -> BlockIndex {
        self.block_index.clone()
    }

    /// Debug utility to validate block index integrity
    ///
    /// Iterates through all items in the block index and verifies that each entry's
    /// position matches its block height, detecting potential synchronization issues.
    pub fn print_items(&self, db: DatabaseProvider) {
        let bi = &self.block_index;
        let tx = match db.tx() {
            Ok(tx) => tx,
            Err(e) => {
                error!(
                    "Failed to open db transaction for printing block index: {:?}",
                    e
                );
                return;
            }
        };
        for i in 0..bi.num_blocks() {
            let Some(item) = bi.get_item(i) else {
                error!("Block index missing item at position {}", i);
                continue;
            };
            let block_hash = item.block_hash;
            let block = match block_header_by_hash(&tx, &block_hash, false) {
                Ok(Some(block)) => block,
                Ok(None) => {
                    error!("Block header not found in DB for hash {}", block_hash);
                    continue;
                }
                Err(e) => {
                    error!("DB error fetching block header for {}: {:?}", block_hash, e);
                    continue;
                }
            };
            debug!("index: {} height: {} hash: {}", i, block.height, block_hash);
            if i != block.height {
                error!("Block index and height do not match!");
            }
        }
    }

    pub fn get_latest_item_cloned(&self) -> Option<BlockIndexItem> {
        self.block_index.get_latest_item()
    }

    pub fn latest_height(&self) -> u64 {
        self.block_index.latest_height()
    }
}
