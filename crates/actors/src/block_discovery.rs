use crate::{block_tree::BlockTreeActor, epoch_service::EpochServiceActor, mempool::MempoolActor};
use actix::prelude::*;
use irys_database::{tx_header_by_txid, BlockIndex, Initialized, Ledger};
use irys_types::{DatabaseProvider, IrysBlockHeader, IrysTransactionHeader};
use reth_db::Database;
use std::sync::{Arc, RwLock};
use tracing::error;

/// BlockDiscoveryActor listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryActor {
    /// Shared access to the block index used for validation.
    pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    /// Manages forks at the head of the chain before finalization
    pub block_tree: Addr<BlockTreeActor>,
    /// Address of the EpochServiceActor for managing epoch-related tasks.
    pub epoch_service: Addr<EpochServiceActor>,
    /// Reference to the mempool actor, which maintains the validity of pending transactions.
    pub mempool: Addr<MempoolActor>,
    /// Database provider for accessing transaction headers and related data.
    pub db: DatabaseProvider,
}

/// When a block is discovered, either produced locally or received from
/// a network peer, this message is broadcast.
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BlockDiscoveredMessage(pub Arc<IrysBlockHeader>);

/// Sent when a discovered block is pre-validated
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
pub struct BlockPreValidatedMessage(
    pub Arc<IrysBlockHeader>,
    pub Arc<Vec<IrysTransactionHeader>>,
);

impl Actor for BlockDiscoveryActor {
    type Context = Context<Self>;
}

impl BlockDiscoveryActor {
    /// Initializes a new BlockDiscoveryActor
    pub fn new(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        block_tree: Addr<BlockTreeActor>,
        epoch_service: Addr<EpochServiceActor>,
        mempool: Addr<MempoolActor>,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            block_index,
            block_tree,
            epoch_service,
            mempool,
            db,
        }
    }
}

impl Handler<BlockDiscoveredMessage> for BlockDiscoveryActor {
    type Result = ();
    fn handle(&mut self, msg: BlockDiscoveredMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Validate discovered block
        let new_block_header = msg.0;

        // Get all the submit ledger transactions for the new block, error if not found
        // TODO: in the future we'll retrieve the missing transactions from the block
        // producer and validate them.
        let submit_txs = match new_block_header.ledgers[Ledger::Submit]
            .txids
            .iter()
            .map(|txid| {
                self.db
                    .view_eyre(|tx| tx_header_by_txid(tx, txid))
                    .and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(txs) => txs,
            Err(e) => {
                error!("Failed to collect tx headers: {}", e);
                return;
            }
        };

        // TODO: Do pre-validation steps here

        self.block_tree.do_send(BlockPreValidatedMessage(
            new_block_header,
            Arc::new(submit_txs),
        ));
    }
}
