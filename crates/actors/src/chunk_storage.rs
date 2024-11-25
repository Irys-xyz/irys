use actix::prelude::*;
use irys_types::{IrysBlockHeader, IrysTransactionHeader};
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
pub struct ChunkStorageActor {}

impl Actor for ChunkStorageActor {
    type Context = Context<Self>;
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

impl ChunkStorageActor {
    /// Populates the relevant indexes to prepare storage modules for new chunks
    ///  when a block is finalized
    pub fn handle_finalized_block(
        block_header: &IrysBlockHeader,
        txs: &Vec<IrysTransactionHeader>,
    ) {
    }
}
