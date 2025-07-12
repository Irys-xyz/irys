use crate::BlockFinalizedMessage;
use actix::prelude::*;
use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockIndex};
use irys_types::{
    BlockIndexItem, ConsensusConfig, IrysBlockHeader, IrysTransactionHeader, H256, U256,
};

use std::sync::{Arc, RwLock};
use tracing::error;

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
        let binding = self.block_index.clone().unwrap();
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
        }
    }

    /// Adds a finalized block and its associated transactions to the block index.
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
    /// * `block` - The finalized block header to be added
    /// * `all_txs` - Complete list of transaction headers, where the first `n` entries
    ///               correspond to the submit ledger's transaction IDs
    pub fn add_finalized_block(
        &mut self,
        block: &Arc<IrysBlockHeader>,
        all_txs: &Arc<Vec<IrysTransactionHeader>>,
    ) {
        if self.block_index.is_none() {
            error!("block_index service not initialized");
            return;
        }

        let chunk_size = self.chunk_size;

        self.block_index
            .clone()
            .unwrap()
            .write()
            .unwrap()
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

impl Handler<BlockFinalizedMessage> for BlockIndexService {
    type Result = eyre::Result<()>;
    fn handle(&mut self, msg: BlockFinalizedMessage, _: &mut Context<Self>) -> Self::Result {
        // Collect working variables to move into the closure
        let block = msg.block_header;
        let all_txs = msg.all_txs;

        // Do something with the block
        self.add_finalized_block(&block, &all_txs);

        Ok(())
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

        let binding = self.block_index.clone().unwrap();
        let bi = binding.read().unwrap();
        let block_height = bi.num_blocks().max(1) - 1;
        Some(bi.get_item(block_height)?.clone())
    }
}
