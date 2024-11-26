use std::sync::{Arc, RwLock};

use actix::prelude::*;
use irys_config::chain::StorageConfig;
use irys_database::{BlockIndex, Initialized, Ledger};
use irys_storage::ii;
use irys_types::{IrysBlockHeader, IrysTransactionHeader, TransactionLedger};
use tracing::info;

use crate::block_producer::BlockFinalizedMessage;

/// Central coordinator for chunk storage operations.
///
/// Responsibilities:
/// - Routes chunks to appropriate storage modules
/// - Maintains chunk location indices
/// - Coordinates chunk reads/writes
/// - Manages storage state transitions
#[derive(Debug)]
pub struct ChunkStorageActor {
    /// Global block index for block bounds/offset tracking
    pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    pub storage_config: Arc<StorageConfig>,
}

impl Actor for ChunkStorageActor {
    type Context = Context<Self>;
}

impl ChunkStorageActor {
    /// Creates a new chunk storage actor
    pub fn new(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        storage_config: Arc<StorageConfig>,
    ) -> Self {
        Self {
            block_index,
            storage_config,
        }
    }

    /// Populates the relevant indexes to prepare storage modules for new chunks
    ///  when a block is finalized
    pub fn handle_finalized_block(
        &mut self,
        block_header: &IrysBlockHeader,
        txs: &Vec<IrysTransactionHeader>,
    ) {
        // Look up the start offset and size of the previous block in the block index
        let index_reader = self.block_index.read().unwrap();
        let start_chunk_offset: u64;

        if block_header.height > 0 {
            let prev_item = index_reader
                .get_item(block_header.height as usize - 1)
                .unwrap();

            let prev_block_info = &prev_item.ledgers[Ledger::Submit as usize];
            start_chunk_offset = prev_block_info.max_chunk_offset;
        } else {
            start_chunk_offset = 0;
        }

        // Calculate the tx_root from the txs and validate the order with whats in the block header
        let (tx_root, proofs) = TransactionLedger::merklize_tx_root(txs);

        // validate with the block header
        if tx_root != block_header.ledgers[Ledger::Submit].tx_root {
            // Panic
        }

        let chunk_interval = ii(
            start_chunk_offset,
            block_header.ledgers[Ledger::Submit].max_chunk_offset,
        );

        // loop though each tx_path and set up all the storage module indexes
        let chunk_size = self.storage_config.chunk_size as usize;
        let mut chunk_offset_cursor = chunk_interval.start() as usize;
        for tx_path in proofs {
            let num_chunks_in_tx = tx_path.offset / chunk_size;

            // For each chunk_offset in the tx_path
            for i in 0..num_chunks_in_tx {
                chunk_offset_cursor += i;

                //  - Find the storage module responsible for storing the chunk offset

                //	- add the tx path bytes and path_hash to the correct index

                //  - add the  tx_path_hash and chunk_path_hash to the chunk_offset index
            }
        }

        // loop though all the chunk_offsets added by this block
        for i in chunk_interval.start()..=chunk_interval.end() {
            //
            //		- get their chunk path hash keys
            //
            //		- attempt to retrieve the chunk bytes from the mempool and add them to the storage module
        }
    }
}

impl Handler<BlockFinalizedMessage> for ChunkStorageActor {
    type Result = ();
    fn handle(&mut self, msg: BlockFinalizedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Access the block header through msg.0
        let block = &msg.0;
        let data_tx = &msg.1;

        // Do something with the block
        info!(
            "Finalized: Block height: {} num tx: {}",
            block.height,
            data_tx.len()
        );
    }
}
