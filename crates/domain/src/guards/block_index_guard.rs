use actix::MessageResponse;
use base58::ToBase58 as _;
use irys_database::block_header_by_hash;
use irys_types::{DatabaseProvider, H256};
use reth_db::Database as _;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use tracing::{debug, error};

use crate::BlockIndex;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
/// As soon as `block_index` is no longer an actix service this `MessageResponse` and the corresponding
/// actix dependency in Cargo.toml can be dropped
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockIndexReadGuard {
    block_index_data: Arc<RwLock<BlockIndex>>,
}

impl BlockIndexReadGuard {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(block_index_data: Arc<RwLock<BlockIndex>>) -> Self {
        Self { block_index_data }
    }

    /// Accessor method to get a read guard for Ledgers
    pub fn read(&self) -> RwLockReadGuard<'_, BlockIndex> {
        self.block_index_data.read().unwrap()
    }

    /// Debug utility to validate block index integrity
    ///
    /// Iterates through all items in the block index and verifies that each entry's
    /// position matches its block height, detecting potential synchronization issues.
    /// This helps identify corrupted index state where the array position doesn't
    /// match the expected block height, which would indicate data inconsistency.
    ///
    /// @param db Database provider for accessing the blockchain data
    pub fn print_items(&self, db: DatabaseProvider) {
        let rg = self.read();
        let tx = db.tx().unwrap();
        for i in 0..rg.num_blocks() {
            let item = rg.get_item(i).unwrap();
            let block_hash = item.block_hash;
            let block = block_header_by_hash(&tx, &block_hash, false)
                .unwrap()
                .unwrap();
            debug!(
                "index: {} height: {} hash: {}",
                i,
                block.height,
                block_hash.0.to_base58()
            );
            if i != block.height {
                error!("Block index and height do not match!");
            }
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    /// Accessor method to get a write guard for the `block_index`
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, BlockIndex> {
        self.block_index_data.write().unwrap()
    }
}

impl irys_types::block_provider::BlockIndex for BlockIndexReadGuard {
    fn contains_block(&self, block_height: u64, block_hash: H256) -> bool {
        let binding = self.read();
        if let Some(item) = binding.get_item(block_height) {
            item.block_hash == block_hash
        } else {
            false
        }
    }
}
