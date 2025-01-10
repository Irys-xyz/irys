use crate::{
    block_index::BlockIndexReadGuard, block_tree::BlockTreeActor, block_validation::block_is_valid,
    epoch_service::PartitionAssignmentsReadGuard, mempool::MempoolActor,
};
use actix::prelude::*;
use irys_database::{block_header_by_hash, tx_header_by_txid, Ledger};
use irys_types::{
    DatabaseProvider, DifficultyAdjustmentConfig, IrysBlockHeader, IrysTransactionHeader,
    StorageConfig, VDFStepsConfig,
};
use reth_db::Database;
use std::sync::Arc;
use tracing::{error, info};

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
    /// Reference to global difficulty config
    pub difficulty_config: DifficultyAdjustmentConfig,
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
    /// Initializes a new `BlockDiscoveryActor`
    pub const fn new(
        block_index_guard: BlockIndexReadGuard,
        partition_assignments_guard: PartitionAssignmentsReadGuard,
        block_tree: Addr<BlockTreeActor>,
        mempool: Addr<MempoolActor>,
        storage_config: StorageConfig,
        difficulty_config: DifficultyAdjustmentConfig,
        db: DatabaseProvider,
        vdf_config: VDFStepsConfig,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            block_tree,
            mempool,
            storage_config,
            difficulty_config,
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
        let prev_block_hash = new_block_header.previous_block_hash;

        let previous_block_header = match self
            .db
            .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash))
        {
            Ok(Some(header)) => header,
            other => {
                error!(
                    "Failed to get block header for hash {}: {:?}",
                    prev_block_hash, other
                );
                return;
            }
        };

        //====================================
        // Submit ledger TX validation
        //------------------------------------
        // Get all the submit ledger transactions for the new block, error if not found
        // this is how we validate that the TXIDs in the Submit Ledger are real transactions.
        // If they are in our mempool and validated we know they are real, if not we have
        // to retrieve and validate them from the block producer.
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
                error!("Failed to collect submit tx headers: {}", e);
                return;
            }
        };

        //====================================
        // Publish ledger TX Validation
        //------------------------------------
        // 1. Validate the proof
        // 2. Validate the transaction
        // 3. Update the local tx headers index so include the ingress- proof.
        //    This keeps the transaction from getting re-promoted each block.
        //    (this last step performed in mempool after the block is confirmed)
        let publish_txs = match new_block_header.ledgers[Ledger::Publish]
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
                error!("Failed to collect publish tx headers: {}", e);
                return;
            }
        };

        if !publish_txs.is_empty() {
            let publish_proofs = match &new_block_header.ledgers[Ledger::Publish].proofs {
                Some(proofs) => proofs,
                None => {
                    error!("Ingress proofs missing");
                    return;
                }
            };

            // Pre-Validate the ingress-proof by verifying the signature
            for (i, tx_header) in publish_txs.iter().enumerate() {
                if let Err(e) = publish_proofs.0[i].pre_validate(&tx_header.data_root) {
                    error!("Invalid ingress proof signature: {}", e);
                    return;
                }
            }
        }

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let block_tree_addr = self.block_tree.clone();
        let storage_config = &self.storage_config;
        let difficulty_config = &self.difficulty_config;

        info!(
            "Validating block height: {} step: {} output: {} prev output: {}",
            new_block_header.height,
            new_block_header.vdf_limiter_info.global_step_number,
            new_block_header.vdf_limiter_info.output,
            new_block_header.vdf_limiter_info.prev_output
        );
        match block_is_valid(
            &new_block_header,
            &previous_block_header,
            &block_index_guard,
            &partitions_guard,
            storage_config,
            difficulty_config,
            &self.vdf_config,
            &new_block_header.miner_address,
        ) {
            Ok(_) => {
                info!("Block is valid, sending to block tree");
                let mut all_txs = submit_txs;
                all_txs.extend_from_slice(&publish_txs);
                block_tree_addr.do_send(BlockPreValidatedMessage(
                    new_block_header,
                    Arc::new(all_txs),
                ));
            }
            Err(err) => {
                error!("Block validation error {:?}", err);
            }
        }
    }
}
