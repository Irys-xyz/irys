use crate::{calculate_chunks_added, BlockConfirmedMessage};
use actix::prelude::*;
use irys_database::{BlockIndex, BlockIndexItem, Initialized, Ledger, LedgerIndexItem};
use irys_types::{IrysBlockHeader, IrysTransactionHeader, StorageConfig, H256, U256};
use std::{
    sync::{Arc, RwLock, RwLockReadGuard},
    time::Duration,
};

//==============================================================================
// BlockIndexReadGuard
//------------------------------------------------------------------------------

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockIndexReadGuard {
    block_index_data: Arc<RwLock<BlockIndex<Initialized>>>,
}

impl BlockIndexReadGuard {
    /// Creates a new `ReadGard` for Ledgers
    pub const fn new(block_index_data: Arc<RwLock<BlockIndex<Initialized>>>) -> Self {
        Self { block_index_data }
    }

    /// Accessor method to get a read guard for Ledgers
    pub fn read(&self) -> RwLockReadGuard<'_, BlockIndex<Initialized>> {
        self.block_index_data.read().unwrap()
    }
}

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "BlockIndexReadGuard")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetBlockIndexGuardMessage;

impl Handler<GetBlockIndexGuardMessage> for BlockIndexActor {
    type Result = BlockIndexReadGuard; // Return guard directly

    fn handle(
        &mut self,
        _msg: GetBlockIndexGuardMessage,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        BlockIndexReadGuard::new(self.block_index.clone())
    }
}

/// The Mempool oversees pending transactions and validation of incoming tx.
/// This actor primarily serves as a wrapper for nested `block_index_data` struct
/// allowing it to receive to actix messages and update its state.
#[derive(Debug)]
pub struct BlockIndexActor {
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    block_log: Vec<BlockLogEntry>,
    num_blocks: u64,
    storage_config: StorageConfig,
}

#[derive(Debug)]
struct BlockLogEntry {
    #[allow(dead_code)]
    pub block_hash: H256,
    pub height: u64,
    pub timestamp: u128,
    pub difficulty: U256,
}

impl Actor for BlockIndexActor {
    type Context = Context<Self>;
}

impl BlockIndexActor {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a `DatabaseEnv`
    pub const fn new(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        storage_config: StorageConfig,
    ) -> Self {
        Self {
            block_index,
            block_log: Vec::new(),
            num_blocks: 0,
            storage_config,
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
    fn add_finalized_block(
        &mut self,
        block: &Arc<IrysBlockHeader>,
        all_txs: &Arc<Vec<IrysTransactionHeader>>,
    ) {
        let chunk_size = self.storage_config.chunk_size;

        // Extract just the transactions referenced in the submit ledger
        let submit_tx_count = block.ledgers[Ledger::Submit].txids.len();
        let submit_txs = &all_txs[..submit_tx_count];

        // Extract just the transactions referenced in the publish ledger
        let publish_txs = &all_txs[submit_tx_count..];

        // TODO: abstract this in the future to work for an arbitrary number of ledgers
        let sub_chunks_added = calculate_chunks_added(submit_txs, chunk_size);
        let pub_chunks_added = calculate_chunks_added(publish_txs, chunk_size);

        let mut index = self.block_index.write().unwrap();

        // Get previous ledger sizes or default to 0 for genesis
        let (max_publish_chunks, max_submit_chunks) =
            if index.num_blocks() == 0 && block.height == 0 {
                (0, sub_chunks_added)
            } else {
                let prev_block = index.get_item((block.height - 1) as usize).unwrap();
                (
                    prev_block.ledgers[Ledger::Publish].max_chunk_offset + pub_chunks_added,
                    prev_block.ledgers[Ledger::Submit].max_chunk_offset + sub_chunks_added,
                )
            };

        let block_index_item = BlockIndexItem {
            block_hash: block.block_hash,
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: max_publish_chunks,
                    tx_root: block.ledgers[Ledger::Publish].tx_root,
                },
                LedgerIndexItem {
                    max_chunk_offset: max_submit_chunks,
                    tx_root: block.ledgers[Ledger::Submit].tx_root,
                },
            ],
        };

        index.push_item(&block_index_item);

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

        if self.num_blocks % 10 == 0 {
            let mut prev_entry: Option<&BlockLogEntry> = None;
            println!("block_height, block_time(ms), difficulty");
            for entry in &self.block_log {
                let duration = if let Some(pe) = prev_entry {
                    Duration::from_millis((entry.timestamp - pe.timestamp) as u64)
                } else {
                    Duration::from_millis(0)
                };
                println!("{}, {:?}, {}", entry.height, duration, entry.difficulty);
                prev_entry = Some(entry);
            }
        }
    }
}

impl Handler<BlockConfirmedMessage> for BlockIndexActor {
    type Result = ();

    // TODO: We are treating confirmed blocks like finalized blocks here when we
    // can receive multiple confirmed blocks on different forks we'll have to
    // listen to a FinalizedBlockMessage
    fn handle(&mut self, msg: BlockConfirmedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Access the block header through msg.0
        let irys_block_header = &msg.0;
        let all_txs = &msg.1;

        // Do something with the block
        self.add_finalized_block(irys_block_header, all_txs);
    }
}

/// Returns the current block height in the index
#[derive(Message, Clone, Debug)]
#[rtype(result = "Option<BlockIndexItem>")]
pub struct GetLatestBlockIndexMessage {}

impl Handler<GetLatestBlockIndexMessage> for BlockIndexActor {
    type Result = Option<BlockIndexItem>;
    fn handle(&mut self, msg: GetLatestBlockIndexMessage, ctx: &mut Self::Context) -> Self::Result {
        let _ = ctx;
        let _ = msg;

        let bi = self.block_index.read().unwrap();
        let block_height = bi.num_blocks().max(1) as usize - 1;
        Some(bi.get_item(block_height)?.clone())
    }
}
