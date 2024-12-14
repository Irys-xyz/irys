use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use actix::prelude::*;
use irys_database::{BlockIndex, BlockIndexItem, Initialized, Ledger, LedgerIndexItem};
use irys_types::{IrysBlockHeader, IrysTransactionHeader, CHUNK_SIZE, H256, U256};

use crate::block_producer::BlockConfirmedMessage;

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct BlockIndexActor {
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    block_log: Vec<BlockLogEntry>,
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
    pub fn new(block_index: Arc<RwLock<BlockIndex<Initialized>>>) -> Self {
        Self {
            block_index,
            block_log: Vec::new(),
        }
    }

    /// Adds a finalized block to the index
    fn add_finalized_block(
        &mut self,
        irys_block_header: &Arc<IrysBlockHeader>,
        data_txs: &Arc<Vec<IrysTransactionHeader>>,
    ) {
        // Calculate total bytes needed, rounding each tx up to nearest chunk size
        // Example: data_size 300KB with 256KB chunks:
        // (300KB + 256KB - 1) / 256KB = 2 chunks -> 2 * 256KB = 512KB total
        let bytes_added = data_txs
            .iter()
            .map(|tx| ((tx.data_size + CHUNK_SIZE - 1) / CHUNK_SIZE) * CHUNK_SIZE)
            .sum::<u64>();

        let chunks_added = bytes_added / CHUNK_SIZE;

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

        if self.block_log.len() % 10 == 0 {
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
