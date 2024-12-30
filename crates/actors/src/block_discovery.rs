use crate::{
    block_index::BlockIndexReadGuard, block_tree::BlockTreeActor, block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard, mempool::MempoolActor,
};
use actix::prelude::*;
use irys_database::{tx_header_by_txid, Ledger};
use irys_types::{DatabaseProvider, IrysBlockHeader, IrysTransactionHeader, StorageConfig};
use reth_db::Database;
use std::sync::Arc;
use tracing::error;

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryActor {
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub partition_assignments_guard: PartitionAssignmentsReadGuard,
    /// Manages forks at the head of the chain before finalization
    pub block_tree: Addr<BlockTreeActor>,
    /// Reference to the mempool actor, which maintains the validity of pending transactions.
    pub mempool: Addr<MempoolActor>,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
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
    /// Initializes a new `BlockDiscoveryActor`
    pub const fn new(
        block_index_guard: BlockIndexReadGuard,
        partition_assignments_guard: PartitionAssignmentsReadGuard,
        block_tree: Addr<BlockTreeActor>,
        mempool: Addr<MempoolActor>,
        storage_config: StorageConfig,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            block_tree,
            mempool,
            storage_config,
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

        let poa = new_block_header.poa.clone();
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let block_tree_addr = self.block_tree.clone();
        let storage_config = &self.storage_config;

        match poa_is_valid(&poa, &block_index_guard, &partitions_guard, storage_config, &new_block_header.reward_address) {
            Ok(_) => {
                block_tree_addr.do_send(BlockPreValidatedMessage(
                    new_block_header,
                    Arc::new(submit_txs),
                ));
            }
            Err(err) => {
                error!("PoA error {:?}", err);
            }
        }
    }
}
