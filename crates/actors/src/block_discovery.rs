use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::BlockTreeService,
    block_validation::prevalidate_block,
    epoch_service::{EpochServiceActor, NewEpochMessage, PartitionAssignmentsReadGuard},
    services::ServiceSenders,
    CommitmentCacheInner, CommitmentCacheMessage, CommitmentStatus, GetCommitmentStateGuardMessage,
};
use actix::prelude::*;
use base58::ToBase58;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, tx_header_by_txid, DataLedger, SystemLedger,
};
use irys_types::{
    CommitmentTransaction, Config, DatabaseProvider, GossipData, H256List, IrysBlockHeader,
    IrysTransactionHeader,
};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth_db::Database;
use std::sync::Arc;
use tracing::{debug, error, info};

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryActor {
    /// Tracks the global state of partition assignments on the protocol
    pub epoch_service: Addr<EpochServiceActor>,
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub partition_assignments_guard: PartitionAssignmentsReadGuard,
    /// Reference to the global config
    pub config: Config,
    /// Database provider for accessing transaction headers and related data.
    pub db: DatabaseProvider,
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
#[rtype(result = "eyre::Result<()>")]
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
        config: Config,
        db: DatabaseProvider,
        vdf_steps_guard: VdfStepsReadGuard,
        service_senders: ServiceSenders,
        epoch_service: Addr<EpochServiceActor>,
        gossip_sender: tokio::sync::mpsc::Sender<GossipData>,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            db,
            vdf_steps_guard,
            service_senders,
            gossip_sender,
            config,
            epoch_service,
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
                        // the previous blocks header was not found in the database
                        "Failed to get previous block header. Previous block hash: {}: {:?}",
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
                        opt.ok_or_else(|| {
                            eyre::eyre!("No tx header found for txid {:?}", txid.0.to_base58())
                        })
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
        // Commitment ledger TX Validation
        //------------------------------------
        // Extract the Commitment ledger from the epoch block
        let commitment_ledger = new_block_header
            .system_ledgers
            .iter()
            .find(|b| b.ledger_id == SystemLedger::Commitment);

        // Validate commitments (if there are some)
        let mut commitments: Vec<CommitmentTransaction> = Vec::new();
        let mut commitment_txids: H256List = H256List::new();
        if let Some(commitment_ledger) = commitment_ledger {
            debug!("{:#?}", commitment_ledger);
            let read_tx = self.db.tx().expect("to create a database read tx");

            // Collect commitments with proper error handling
            match commitment_ledger
                .tx_ids
                .iter()
                .map(|txid| {
                    commitment_tx_by_txid(&read_tx, txid).and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No commitment tx found for txid {:?}", txid))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(collected) => {
                    commitments = collected;
                    commitment_txids = commitment_ledger.tx_ids.clone();
                }

                Err(e) => error!("Failed to collect commitment transactions: {:?}", e),
            }
        }

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_index_guard2 = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let block_tree_addr = BlockTreeService::from_registry();
        let config = self.config.clone();
        let vdf_steps_guard = self.vdf_steps_guard.clone();
        let db = self.db.clone();
        let ema_service_sender = self.service_senders.ema.clone();
        let commitment_cache_sender = self.service_senders.commitment_cache.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();
        let epoch_service = self.epoch_service.clone();
        let epoch_config = self.config.consensus.epoch.clone();

        info!(height = ?new_block_header.height,
            global_step_counter = ?new_block_header.vdf_limiter_info.global_step_number,
            output = ?new_block_header.vdf_limiter_info.output,
            prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "Validating block"
        );

        let gossip_sender = self.gossip_sender.clone();
        Box::pin(async move {
            info!("Pre-validating block");
            let validation_future = tokio::task::spawn_blocking(move || {
                prevalidate_block(
                    block_header,
                    previous_block_header,
                    partitions_guard,
                    config,
                    vdf_steps_guard,
                    ema_service_sender,
                )
            });

            match validation_future.await.unwrap().await {
                Ok(_) => {
                    // Attempt to validate / update the epoch commitment cache
                    for commitment_tx in commitments.iter() {
                        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                        let _ =
                            commitment_cache_sender.send(CommitmentCacheMessage::AddCommitment {
                                commitment_tx: commitment_tx.clone(),
                                response: oneshot_tx,
                            });
                        let status = oneshot_rx
                            .await
                            .expect("to receive CommitmentStatus from AddCommitment message");

                        if matches!(status, CommitmentStatus::Accepted) == false {
                            // Something went wrong with the commitments validation, it's time to roll back
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            let _ = commitment_cache_sender.send(
                                CommitmentCacheMessage::RollbackCommitments {
                                    commitment_txs: commitment_txids,
                                    response: tx,
                                },
                            );
                            let _ = rx
                                .await
                                .expect("to receive a response from RollbackCommitments message");

                            // These commitments do not result in valid commitment state
                            return Err(eyre::eyre!("Invalid commitments"));
                        }
                    }

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
                        .await??;

                    // Check if we've reached the end of an epoch and should finalize commitments
                    let block_height = new_block_header.height;
                    let blocks_in_epoch = epoch_config.num_blocks_in_epoch;
                    let is_epoch_block = block_height > 0 && block_height % blocks_in_epoch == 0;

                    if is_epoch_block {
                        // For epoch blocks, validate that all included commitments are legitimate
                        // Get current commitment state from epoch service for validation
                        let commitment_state_guard = epoch_service
                            .send(GetCommitmentStateGuardMessage)
                            .await
                            .unwrap();

                        // Create a temporary local commitment validation environment
                        // This avoids async overhead while checking commitment validity and creates
                        // an independent cache we can populate and discard
                        let mut local_commitment_cache =
                            CommitmentCacheInner::new(commitment_state_guard);

                        // Validate each commitment transaction before accepting the epoch block
                        for commitment_tx in commitments.iter() {
                            let status =
                                local_commitment_cache.add_commitment(commitment_tx.clone());

                            // Reject the entire epoch block if any commitment is invalid
                            // This ensures only verified commitments are finalized at epoch boundaries
                            if status != CommitmentStatus::Accepted {
                                return Err(eyre::eyre!("Invalid commitments in epoch block"));
                            }
                        }

                        // Look up the previous epoch block
                        let block_item = block_index_guard2
                            .read()
                            .get_item(block_height - blocks_in_epoch)
                            .expect("previous epoch block to be in block index")
                            .clone();

                        let previous_epoch_block = db
                            .view(|tx| block_header_by_hash(tx, &block_item.block_hash, false))
                            .unwrap()
                            .expect("previous epoch block to be in database");

                        // Send the NewEpochMessage referencing the current and previous epoch blocks
                        epoch_service.do_send(NewEpochMessage {
                            previous_epoch_block,
                            epoch_block: new_block_header.clone(),
                            commitments,
                        });

                        // Clear the CommitmentCache for a new epoch
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = commitment_cache_sender
                            .send(CommitmentCacheMessage::ClearCache { response: tx });
                        let _ = rx
                            .await
                            .expect("to receive a response from ClearCache message");
                    }

                    // Send the block to the gossip bus
                    tracing::trace!(
                        "sending block to bus: block height {:?}",
                        &new_block_header.height
                    );
                    if let Err(error) = gossip_sender
                        .send(GossipData::Block(new_block_header.as_ref().clone()))
                        .await
                    {
                        tracing::error!("Failed to send gossip message: {}", error);
                    }

                    Ok(())
                }
                Err(err) => {
                    tracing::error!("Block validation error {:?}", err);
                    Err(eyre::eyre!("Block validation error {:?}", err))
                }
            }
        })
    }
}
