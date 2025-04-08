use crate::{
    block_index_service::BlockIndexReadGuard, block_tree_service::BlockTreeService,
    block_validation::prevalidate_block, epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use actix::prelude::*;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, tx_header_by_txid, DataLedger, SystemLedger,
};
use irys_types::{
    DatabaseProvider, DifficultyAdjustmentConfig, GossipData, IrysBlockHeader,
    IrysTransactionHeader, StorageConfig, VDFStepsConfig,
};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth_db::Database;
use std::sync::Arc;
use tracing::info;

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
    /// Service Senders
    pub service_senders: ServiceSenders,
    /// Gossip message bus
    pub gossip_sender: tokio::sync::mpsc::Sender<GossipData>,
}

/// When a block is discovered, either produced locally or received from
/// a network peer, this message is broadcast.
#[derive(Message, Debug, Clone)]
#[rtype(result = "eyre::Result<()>")]
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
        service_senders: ServiceSenders,
        gossip_sender: tokio::sync::mpsc::Sender<GossipData>,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            storage_config,
            difficulty_config,
            db,
            vdf_config,
            vdf_steps_guard,
            service_senders,
            gossip_sender,
        }
    }
}

impl Handler<BlockDiscoveredMessage> for BlockDiscoveryActor {
    type Result = ResponseFuture<eyre::Result<()>>;

    fn handle(&mut self, msg: BlockDiscoveredMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Validate discovered block
        let new_block_header = msg.0;
        let prev_block_hash = new_block_header.previous_block_hash;

        let previous_block_header = match self
            .db
            .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash, false))
        {
            Ok(Some(header)) => header,
            other => {
                return Box::pin(async move {
                    Err(eyre::eyre!(
                        "Failed to get block header for hash {}: {:?}",
                        prev_block_hash,
                        other
                    ))
                });
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
        let submit_txs = match new_block_header.data_ledgers[DataLedger::Submit]
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
                return Box::pin(async move {
                    Err(eyre::eyre!("Failed to collect submit tx headers: {}", e))
                });
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
        let publish_txs = match new_block_header.data_ledgers[DataLedger::Publish]
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
                return Box::pin(async move {
                    Err(eyre::eyre!("Failed to collect publish tx headers: {}", e))
                });
            }
        };

        if !publish_txs.is_empty() {
            let publish_proofs = match &new_block_header.data_ledgers[DataLedger::Publish].proofs {
                Some(proofs) => proofs,
                None => {
                    return Box::pin(async move { Err(eyre::eyre!("Ingress proofs missing")) });
                }
            };

            // Pre-Validate the ingress-proof by verifying the signature
            for (i, tx_header) in publish_txs.iter().enumerate() {
                if let Err(e) = publish_proofs.0[i].pre_validate(&tx_header.data_root) {
                    return Box::pin(async move {
                        Err(eyre::eyre!("Invalid ingress proof signature: {}", e))
                    });
                }
            }
        }

        //====================================
        // Commitments ledger TX Validation
        //------------------------------------
        // Extract the Commitment ledger from the epoch block
        let commitments_ledger = new_block_header
            .system_ledgers
            .iter()
            .find(|b| b.ledger_id == SystemLedger::Commitment);

        // Validate commitments (if there are some)
        if let Some(commitment_ledger) = commitments_ledger {
            let read_tx = self.db.tx().expect("to create a database read tx");
            let _commitment_txs = commitment_ledger
                .tx_ids
                .iter()
                .map(|txid| {
                    commitment_tx_by_txid(&read_tx, txid).and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No commitment tx found for txid {:?}", txid))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .expect("to be able to retrieve all of the commitment tx headers locally");

            // TODO: Non epoch blocks and epoch blocks treat the commitments ledger a little differently
            // during the epoch, stake and pledge commitments accumulate waiting to be finalized when the
            // next epoch starts. As a result these pending commitments during the epoch need to have
            // their own CommitmentsState where pending pledges can be checked to see if they have an
            // outstanding stake (check with epoch_service) or if they've posted a pending stake commitment.
            //
            // This work will be done next, for now commitments are only handled in the genesis block
        }

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let block_tree_addr = BlockTreeService::from_registry();
        let storage_config = self.storage_config.clone();
        let difficulty_config = self.difficulty_config;
        let vdf_config = self.vdf_config.clone();
        let vdf_steps_guard = self.vdf_steps_guard.clone();
        let db = self.db.clone();
        let ema_service_sender = self.service_senders.ema.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();

        info!(height = ?new_block_header.height,
            global_step_counter = ?new_block_header.vdf_limiter_info.global_step_number,
            output = ?new_block_header.vdf_limiter_info.output,
            prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "Validating block"
        );

        let gossip_sender = self.gossip_sender.clone();
        Box::pin(async move {
            let block_header_clone = new_block_header.clone(); // Clone before moving

            info!("Pre-validating block");
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
                    ema_service_sender,
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
                            new_block_header.clone(),
                            Arc::new(all_txs),
                        ))
                        .await
                        .unwrap();

                    // Send the block to the gossip bus
                    if let Err(error) = gossip_sender
                        .send(GossipData::Block(new_block_header.as_ref().clone()))
                        .await
                    {
                        tracing::error!("Failed to send gossip message: {}", error);
                    }

                    Ok(())
                }
                Err(err) => Err(eyre::eyre!("Block validation error {:?}", err)),
            }
        })
    }
}
