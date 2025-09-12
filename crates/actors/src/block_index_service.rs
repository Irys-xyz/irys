use crate::BlockMigrationMessage;
use actix::prelude::*;
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockIndex};
use irys_types::{
    BlockIndexItem, ConsensusConfig, DataTransactionHeader, IrysBlockHeader, H256, U256,
};

use std::sync::{Arc, RwLock};
use std::collections::BTreeMap;
use tracing::{debug, error, warn};

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "BlockIndexReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetBlockIndexGuardMessage;

impl Handler<GetBlockIndexGuardMessage> for BlockIndexService {
    type Result = BlockIndexReadGuard; // Return guard directly

    fn handle(
        &mut self,
        _msg: GetBlockIndexGuardMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.block_index.is_none() {
            error!("block_index service not initialized");
        }
        let binding = self
            .block_index
            .clone()
            .expect("block_index must be initialized");
        BlockIndexReadGuard::new(binding)
    }
}

//==============================================================================
// BlockIndex Actor
//------------------------------------------------------------------------------

/// The Mempool oversees pending transactions and validation of incoming tx.
/// This actor primarily serves as a wrapper for nested `block_index_data` struct
/// allowing it to receive to actix messages and update its state.
#[derive(Debug, Default)]
pub struct BlockIndexService {
    block_index: Option<Arc<RwLock<BlockIndex>>>,
    block_log: Vec<BlockLogEntry>,
    num_blocks: u64,
    chunk_size: u64,
    /// Buffer of out-of-order migration messages keyed by height
    pending: BTreeMap<u64, (Arc<IrysBlockHeader>, Arc<Vec<DataTransactionHeader>>)>,
}

/// Allows this actor to live in the the local service registry
impl Supervised for BlockIndexService {}

impl SystemService for BlockIndexService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: block_index");
    }
}

#[derive(Debug)]
struct BlockLogEntry {
    #[expect(dead_code)]
    pub block_hash: H256,
    #[expect(dead_code)]
    pub height: u64,
    #[expect(dead_code)]
    pub timestamp: u128,
    #[expect(dead_code)]
    pub difficulty: U256,
}

impl Actor for BlockIndexService {
    type Context = Context<Self>;
}

impl BlockIndexService {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a `DatabaseEnv`
    pub fn new(block_index: Arc<RwLock<BlockIndex>>, consensus_config: &ConsensusConfig) -> Self {
        Self {
            block_index: Some(block_index),
            block_log: Vec::new(),
            num_blocks: 0,
            chunk_size: consensus_config.chunk_size,
            pending: BTreeMap::new(),
        }
    }

    /// Adds a migrated block and its associated transactions to the block index.
    ///
    /// # Safety Considerations
    /// This function expects `all_txs` to contain transaction headers for every transaction ID
    /// in the block's submit ledger and publish Ledger. This invariant is normally guaranteed
    /// by the block validation process, which verifies all transactions before block confirmation.
    /// However, if this function is called with incomplete or unvalidated transaction data,
    /// it may result in:
    /// - Index out of bounds errors when accessing `all_txs`
    /// - Data corruption in the block index
    /// - Invalid chunk calculations
    ///
    /// # Arguments
    /// * `block` - The migrated block header to be added
    /// * `all_txs` - Complete list of transaction headers, where the first `n` entries
    ///               correspond to the submit ledger's transaction IDs
    fn try_process_next_in_order(&mut self) {
        if self.block_index.is_none() {
            error!("block_index service not initialized");
            return;
        }

        // Keep processing contiguous buffered heights starting at the current next expected height
        loop {
            let next_height = {
                let binding = self
                    .block_index
                    .clone()
                    .expect("block_index must be initialized");
                let bi = binding.read().expect("block_index read lock poisoned");
                bi.num_blocks() // next expected height to insert
            };

            if let Some((block, all_txs)) = self.pending.remove(&next_height) {
                debug!(
                    height = block.height,
                    expected = next_height,
                    "processing buffered migration in order"
                );
                self.migrate_block_inner(&block, &all_txs);
            } else {
                break;
            }
        }
    }

    fn migrate_block_inner(
        &mut self,
        block: &Arc<IrysBlockHeader>,
        all_txs: &Arc<Vec<DataTransactionHeader>>,
    ) {
        if self.block_index.is_none() {
            error!("block_index service not initialized");
            return;
        }

        let chunk_size = self.chunk_size;

        self
            .block_index
            .clone()
            .expect("block_index must be initialized")
            .write()
            .expect("block_index write lock poisoned")
            .push_block(block, all_txs, chunk_size)
            .expect("expect to add the block to the index");

        // Block log tracking
        self.block_log.push(BlockLogEntry {
            block_hash: block.block_hash,
            height: block.height,
            timestamp: block.timestamp,
            difficulty: block.diff,
        });

        // Remove oldest entries if we exceed 20
        if self.block_log.len() > 20 {
            self.block_log.drain(0..self.block_log.len() - 20);
        }

        self.num_blocks += 1;

        // if self.num_blocks % 10 == 0 {
        //     let mut prev_entry: Option<&BlockLogEntry> = None;
        //     info!("block_height, block_time(ms), difficulty");
        //     for entry in &self.block_log {
        //         let duration = if let Some(pe) = prev_entry {
        //             if entry.timestamp >= pe.timestamp {
        //                 Duration::from_millis((entry.timestamp - pe.timestamp) as u64)
        //             } else {
        //                 Duration::from_millis(0)
        //             }
        //         } else {
        //             Duration::from_millis(0)
        //         };
        //         info!("{}, {:?}, {}", entry.height, duration, entry.difficulty);
        //         prev_entry = Some(entry);
        //     }
        // }
    }
}

impl Handler<BlockMigrationMessage> for BlockIndexService {
    type Result = eyre::Result<()>;
    fn handle(&mut self, msg: BlockMigrationMessage, _: &mut Context<Self>) -> Self::Result {
        let block = msg.block_header;
        let all_txs = msg.all_txs;

        // Determine next expected height and current index tip for logging/ordering
        let (next_height, latest_height) = if let Some(binding) = self.block_index.clone() {
            let bi = binding.read().expect("block_index read lock poisoned");
            (bi.num_blocks(), bi.latest_height())
        } else {
            (0, 0)
        };

        debug!(
            incoming_height = block.height,
            next_expected_height = next_height,
            latest_index_height = latest_height,
            "received BlockMigrationMessage"
        );

        if block.height == next_height {
            // Process immediately and then drain any contiguous buffered ones
            self.migrate_block_inner(&block, &all_txs);
            self.try_process_next_in_order();
            Ok(())
        } else if block.height > next_height {
            // Buffer until predecessors arrive
            let height = block.height;
            if self.pending.insert(height, (block, all_txs)).is_some() {
                warn!("replaced pending migration at height {}", height);
            }
            debug!(
                buffered_height = height,
                waiting_for = next_height,
                "buffered out-of-order migration"
            );
            Ok(())
        } else {
            // Height is less than next expected; likely duplicate or reorg artifact
            warn!(
                incoming_height = block.height,
                next_expected_height = next_height,
                "received stale migration; dropping"
            );
            Ok(())
        }
    }
}

/// Returns the current block height in the index
#[derive(Message, Clone, Debug)]
#[rtype(result = "Option<BlockIndexItem>")]
pub struct GetLatestBlockIndexMessage {}

impl Handler<GetLatestBlockIndexMessage> for BlockIndexService {
    type Result = Option<BlockIndexItem>;
    fn handle(
        &mut self,
        _msg: GetLatestBlockIndexMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.block_index.is_none() {
            error!("block_index service not initialized");
            return None;
        }

        let binding = self
            .block_index
            .clone()
            .expect("block_index must be initialized");
        let bi = binding.read().expect("block_index read lock poisoned");
        let block_height = bi.num_blocks().max(1) - 1;
        Some(bi.get_item(block_height)?.clone())
    }
}
