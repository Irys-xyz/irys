use crate::{
    block_index_service::BlockIndexReadGuard, block_tree_service::BlockTreeService,
    block_validation::prevalidate_block, epoch_service::PartitionAssignmentsReadGuard,
    vdf_service::VdfStepsReadGuard,
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
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
    /// Reference to global difficulty config
    pub difficulty_config: DifficultyAdjustmentConfig,
    /// Database provider for accessing transaction headers and related data.
    pub db: DatabaseProvider,
    /// VDF configuration for the node
    pub vdf_config: VDFStepsConfig,
    /// Store last VDF Steps
    pub vdf_steps_guard: VdfStepsReadGuard,
}

/// When a block is discovered, either produced locally or received from
/// a network peer, this message is broadcast.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), ()>")]
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
        storage_config: StorageConfig,
        difficulty_config: DifficultyAdjustmentConfig,
        db: DatabaseProvider,
        vdf_config: VDFStepsConfig,
        vdf_steps_guard: VdfStepsReadGuard,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            storage_config,
            difficulty_config,
            db,
            vdf_config,
            vdf_steps_guard,
        }
    }
}

impl Handler<BlockDiscoveredMessage> for BlockDiscoveryActor {
    type Result = ResponseFuture<Result<(), ()>>;
    fn handle(&mut self, msg: BlockDiscoveredMessage, ctx: &mut Context<Self>) -> Self::Result {
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
                return Box::pin(async move { Err(()) });
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
            .tx_ids
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
                return Box::pin(async move { Err(()) });
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
            .tx_ids
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
                return Box::pin(async move { Err(()) });
            }
        };

        if !publish_txs.is_empty() {
            let publish_proofs = match &new_block_header.ledgers[Ledger::Publish].proofs {
                Some(proofs) => proofs,
                None => {
                    error!("Ingress proofs missing");
                    return Box::pin(async move { Err(()) });
                }
            };

            // Pre-Validate the ingress-proof by verifying the signature
            for (i, tx_header) in publish_txs.iter().enumerate() {
                if let Err(e) = publish_proofs.0[i].pre_validate(&tx_header.data_root) {
                    error!("Invalid ingress proof signature: {}", e);
                    return Box::pin(async move { Err(()) });
                }
            }
        }

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let block_tree_addr = BlockTreeService::from_registry();
        let storage_config = self.storage_config.clone();
        let difficulty_config = self.difficulty_config.clone();
        let vdf_config = self.vdf_config.clone();
        let vdf_steps_guard = self.vdf_steps_guard.clone();
        let db = self.db.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();

        info!(
            "Validating block height: {} step: {} output: {} prev output: {}",
            new_block_header.height,
            new_block_header.vdf_limiter_info.global_step_number,
            new_block_header.vdf_limiter_info.output,
            new_block_header.vdf_limiter_info.prev_output
        );
        Box::pin(async move {
            let block_header_clone = new_block_header.clone(); // Clone before moving

            let validation_future = tokio::task::spawn_blocking(move || {
                prevalidate_block(
                    block_header,
                    previous_block_header,
                    block_index_guard,
                    partitions_guard,
                    storage_config,
                    difficulty_config,
                    vdf_config,
                    vdf_steps_guard,
                    block_header_clone.miner_address, // Use clone in validation
                )
            });

            match validation_future.await.unwrap().await {
                Ok(_) => {
                    info!("Block is valid, sending to block tree");

                    db.update_eyre(|tx| irys_database::insert_block_header(tx, &new_block_header))
                        .unwrap();

                    let mut all_txs = submit_txs;
                    all_txs.extend_from_slice(&publish_txs);
                    block_tree_addr
                        .send(BlockPreValidatedMessage(
                            new_block_header,
                            Arc::new(all_txs),
                        ))
                        .await
                        .unwrap();
                    Ok(())
                }
                Err(err) => {
                    error!("Block validation error {:?}", err);
                    Err(())
                }
            }
        })
    }
}
