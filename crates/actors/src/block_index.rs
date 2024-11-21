use std::sync::{Arc, RwLock};

use actix::prelude::*;
use irys_database::{BlockIndex, BlockIndexItem, Initialized, Ledger, LedgerIndexItem};
use irys_types::{IrysBlockHeader, IrysTransactionHeader, CHUNK_SIZE, H256};

use crate::block_producer::BlockConfirmedMessage;

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct BlockIndexActor {
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
}

impl Actor for BlockIndexActor {
    type Context = Context<Self>;
}

impl BlockIndexActor {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a DatabaseEnv
    pub fn new(block_index: Arc<RwLock<BlockIndex<Initialized>>>) -> Self {
        Self { block_index }
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

        let mut index = self.block_index.write().unwrap();

        // Get previous ledger sizes or default to 0 for genesis
        let (publish_size, submit_size) =
            if index.num_blocks() == 0 && irys_block_header.height == 0 {
                (0, bytes_added as u128)
            } else {
                let prev_block = index.get_item(0).unwrap();
                (
                    prev_block.ledgers[Ledger::Publish as usize].ledger_size,
                    prev_block.ledgers[Ledger::Submit as usize].ledger_size + bytes_added as u128,
                )
            };

        let block_index_item = BlockIndexItem {
            block_hash: irys_block_header.block_hash,
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    ledger_size: publish_size,
                    tx_root: H256::zero(),
                },
                LedgerIndexItem {
                    ledger_size: submit_size,
                    tx_root: H256::zero(),
                },
            ],
        };

        index.push_item(&block_index_item);
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
#[rtype(result = "u64")]
pub struct GetBlockHeightMessage {}

impl Handler<GetBlockHeightMessage> for BlockIndexActor {
    type Result = u64;

    fn handle(&mut self, msg: GetBlockHeightMessage, ctx: &mut Self::Context) -> Self::Result {
        let _ = ctx;
        let _ = msg;

        self.block_index.read().unwrap().num_blocks()
    }
}
