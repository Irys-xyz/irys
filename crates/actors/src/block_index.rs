use std::{
    sync::{Arc, RwLock, RwLockReadGuard},
    time::Duration,
};

use actix::prelude::*;

use irys_database::{
    BlockBounds, BlockIndex, BlockIndexItem, Initialized, Ledger, LedgerIndexItem,
};
use irys_types::{IrysBlockHeader, IrysTransactionHeader, StorageConfig, H256, U256};

use crate::block_producer::BlockConfirmedMessage;

//==============================================================================
// Read only view of BlockIndex State
//------------------------------------------------------------------------------

/// Retrieve a read only reference to the ledger partition assignments
#[derive(Message, Debug)]
#[rtype(result = "BlockIndexView")] // Remove MessageResult wrapper since type implements MessageResponse
pub struct GetBlockIndexViewMessage;

/// A read only view of Block Index state (stores a reference to original block_index)
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockIndexView {
    inner: Arc<RwLock<BlockIndex<Initialized>>>,
}

impl BlockIndexView {
    /// Create a new read only view from the block index
    pub fn new(block_index: Arc<RwLock<BlockIndex<Initialized>>>) -> Self {
        Self { inner: block_index }
    }

    /// Retrieve the number of blocks in the index
    pub fn num_blocks(&self) -> u64 {
        self.inner.read().unwrap().num_blocks()
    }

    /// Retrieve a block index item based on its height (offset from the 0 based start of the index)
    pub fn get_item(&self, block_height: usize) -> Option<BlockIndexItem> {
        let guard = self.inner.read().unwrap();
        guard.get_item(block_height).cloned()
    }

    /// Given a specific ledger and chunk offset, where did the block containing
    /// that ledger offset that offset start and end, in ledger relative chunk
    /// offsets
    pub fn get_block_bounds(&self, ledger: Ledger, chunk_offset: u64) -> BlockBounds {
        self.inner
            .read()
            .unwrap()
            .get_block_bounds(ledger, chunk_offset)
    }

    /// For a given ledger relative chunk offset, retrieve the block index data
    /// for the block responsible for adding it to the ledger.
    pub fn get_block_index_item(
        &self,
        ledger: Ledger,
        chunk_offset: u64,
    ) -> eyre::Result<(usize, BlockIndexItem)> {
        let guard = self.inner.read().unwrap();
        let (idx, item) = guard.get_block_index_item(ledger, chunk_offset)?;
        Ok((idx, item.clone()))
    }
}

/// The Mempool oversees pending transactions and validation of incoming tx.
/// This actor primarily serves as a wrapper for nested block_index_data struct
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
    /// counted reference to a DatabaseEnv
    pub fn new(
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

    /// Adds a finalized block to the index
    fn add_finalized_block(
        &mut self,
        irys_block_header: &Arc<IrysBlockHeader>,
        data_txs: &Arc<Vec<IrysTransactionHeader>>,
    ) {
        let chunk_size = self.storage_config.chunk_size;

        // Calculate total bytes needed, rounding each tx up to nearest chunk_size
        // Example: data_size 300KB with 256KiB chunks:
        // (300KB + 256KB - 1) / 256KB = 2 chunks -> 2 * 256KB = 512KB total
        let bytes_added = data_txs
            .iter()
            .map(|tx| ((tx.data_size + chunk_size - 1) / chunk_size) * chunk_size)
            .sum::<u64>();

        let chunks_added = bytes_added / chunk_size;

        let mut index = self.block_index.write().unwrap();

        // Get previous ledger sizes or default to 0 for genesis
        let (max_publish_chunks, max_submit_chunks) =
            if index.num_blocks() == 0 && irys_block_header.height == 0 {
                (0, chunks_added)
            } else {
                let prev_block = index.get_item(0).unwrap();
                (
                    prev_block.ledgers[Ledger::Publish as usize].max_chunk_offset,
                    prev_block.ledgers[Ledger::Submit as usize].max_chunk_offset + chunks_added,
                )
            };

        let block_index_item = BlockIndexItem {
            block_hash: irys_block_header.block_hash,
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: max_publish_chunks,
                    tx_root: irys_block_header.ledgers[Ledger::Publish].tx_root,
                },
                LedgerIndexItem {
                    max_chunk_offset: max_submit_chunks,
                    tx_root: irys_block_header.ledgers[Ledger::Submit].tx_root,
                },
            ],
        };

        index.push_item(&block_index_item);

        // Block log tracking
        self.block_log.push(BlockLogEntry {
            block_hash: irys_block_header.block_hash,
            height: irys_block_header.height,
            timestamp: irys_block_header.timestamp,
            difficulty: irys_block_header.diff,
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
                let duration = if let Some(ref pe) = prev_entry {
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
        let data_txs = &msg.1;

        // Do something with the block
        self.add_finalized_block(&irys_block_header, &data_txs);
    }
}

impl Handler<GetBlockIndexViewMessage> for BlockIndexActor {
    type Result = BlockIndexView;

    fn handle(&mut self, _msg: GetBlockIndexViewMessage, _ctx: &mut Self::Context) -> Self::Result {
        BlockIndexView::new(self.block_index.clone())
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
