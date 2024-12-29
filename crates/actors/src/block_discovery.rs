use crate::{
    block_index::BlockIndexView, block_tree::BlockTreeActor, block_validation::block_is_valid,
    epoch_service::PartitionAssignmentsReadGuard, mempool::MempoolActor, vdf,
};
use actix::prelude::*;
use irys_database::{tx_header_by_txid, BlockIndex, Initialized, Ledger};
use irys_types::{DatabaseProvider, IrysBlockHeader, IrysTransactionHeader, StorageConfig, VDFStepsConfig};
use reth::network::error;
use reth_db::Database;
use std::sync::{Arc, RwLock};
use tracing::{error, info};

/// BlockDiscoveryActor listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryActor {
    /// Read only view of the block index
    pub block_index_view: BlockIndexView,
    /// PartitionAssignmentsReadGuard for looking up ledger info
    pub partition_assignments_view: PartitionAssignmentsReadGuard,
    /// Manages forks at the head of the chain before finalization
    pub block_tree: Addr<BlockTreeActor>,
    /// Reference to the mempool actor, which maintains the validity of pending transactions.
    pub mempool: Addr<MempoolActor>,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
    /// Database provider for accessing transaction headers and related data.
    pub db: DatabaseProvider,
    /// VDF configuration for the node
    pub vdf_config: VDFStepsConfig,
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
        block_index_view: BlockIndexView,
        partition_assignments_view: PartitionAssignmentsReadGuard,
        block_tree: Addr<BlockTreeActor>,
        mempool: Addr<MempoolActor>,
        storage_config: StorageConfig,
        db: DatabaseProvider,
        vdf_config: VDFStepsConfig,
    ) -> Self {
        Self {
            block_index_view,
            partition_assignments_view,
            block_tree,
            mempool,
            storage_config,
            db,
            vdf_config,
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

        let block_index_view = self.block_index_view.clone();
        let partitions_view = self.partition_assignments_view.clone();
        let block_tree_addr = self.block_tree.clone();
        let storage_config = &self.storage_config;

        info!("Validating block height:{} step:{} output:{} prev output: {}", new_block_header.height, new_block_header.vdf_limiter_info.global_step_number, new_block_header.vdf_limiter_info.output, new_block_header.vdf_limiter_info.prev_output);
        match block_is_valid(&new_block_header, &block_index_view, &partitions_view, storage_config, &self.vdf_config) {
            Ok(_) => {
                info!("Block is valid, sending to block tree");
                block_tree_addr.do_send(BlockPreValidatedMessage(
                    new_block_header,
                    Arc::new(submit_txs),
                ));
            }
            Err(err) => {
                error!("Block validation error {:?}", err);
            }
        }
    }
}
