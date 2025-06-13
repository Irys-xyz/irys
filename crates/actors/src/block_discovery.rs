use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::BlockTreeServiceMessage,
    block_validation::prevalidate_block,
    epoch_service::{EpochServiceActor, NewEpochMessage, PartitionAssignmentsReadGuard},
    mempool_service::MempoolServiceMessage,
    services::ServiceSenders,
    CommitmentCacheInner, CommitmentCacheMessage, CommitmentCacheStatus,
    GetCommitmentStateGuardMessage,
};
use actix::prelude::*;
use async_trait::async_trait;
use base58::ToBase58;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _, tx_header_by_txid,
    SystemLedger,
};
use irys_reward_curve::HalvingCurve;
use irys_types::{
    CommitmentTransaction, Config, DataLedger, DatabaseProvider, GossipData, H256List,
    IrysBlockHeader, IrysTransactionHeader, IrysTransactionId,
};
use irys_vdf::state::VdfStateReadonly;
use reth_db::Database;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, error, info, Instrument, Span};

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
    /// The block reward curve
    pub reward_curve: Arc<HalvingCurve>,
    /// Database provider for accessing transaction headers and related data.
    pub db: DatabaseProvider,
    /// Store last VDF Steps
    pub vdf_steps_guard: VdfStateReadonly,
    /// Service Senders
    pub service_senders: ServiceSenders,
    /// Tracing span
    pub span: Span,
}

#[async_trait::async_trait]
pub trait BlockDiscoveryFacade: Clone + Unpin + Send + Sync + 'static {
    async fn handle_block(&self, block: IrysBlockHeader) -> eyre::Result<()>;
}

#[derive(Debug, Clone)]
pub struct BlockDiscoveryFacadeImpl {
    addr: Addr<BlockDiscoveryActor>,
}

impl BlockDiscoveryFacadeImpl {
    pub fn new(addr: Addr<BlockDiscoveryActor>) -> Self {
        Self { addr }
    }
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryFacadeImpl {
    async fn handle_block(&self, block: IrysBlockHeader) -> eyre::Result<()> {
        self.addr
            .send(BlockDiscoveredMessage(Arc::new(block)))
            .await
            .map_err(|mailbox_error: MailboxError| eyre!("MailboxError: {:?}", mailbox_error))?
    }
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

impl Handler<BlockDiscoveredMessage> for BlockDiscoveryActor {
    type Result = ResponseFuture<eyre::Result<()>>;

    fn handle(&mut self, msg: BlockDiscoveredMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let span = self.span.clone();
        let _span = span.enter();
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
        // Block header pre-validation
        //------------------------------------
        let block_index_guard2 = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let ema_service_sender = self.service_senders.ema.clone();
        let commitment_cache_sender = self.service_senders.commitment_cache.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();
        let epoch_service = self.epoch_service.clone();
        let epoch_config = self.config.consensus.epoch.clone();
        let span2 = self.span.clone();
        let block_tree_sender = self.service_senders.block_tree.clone();
        let mempool_sender = self.service_senders.mempool.clone();

        info!(height = ?new_block_header.height,
            global_step_counter = ?new_block_header.vdf_limiter_info.global_step_number,
            output = ?new_block_header.vdf_limiter_info.output,
            prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "Validating block"
        );

        let gossip_sender = self.service_senders.gossip_broadcast.clone();
        let reward_curve = Arc::clone(&self.reward_curve);
        Box::pin(async move {
            let span3 = span2.clone();
            let _span = span3.enter();

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
            let mut commitment_tx_ids: H256List = H256List::new();
            if let Some(commitment_ledger) = commitment_ledger {
                debug!("{:#?}", commitment_ledger);
                commitment_tx_ids = commitment_ledger.tx_ids.clone();
                match get_commitment_tx_in_parallel(
                    commitment_tx_ids.0.clone(),
                    &mempool_sender,
                    &db,
                )
                .await
                {
                    Ok(tx) => {
                        commitments = tx;
                    }
                    Err(e) => error!("Failed to collect commitment transactions: {:?}", e),
                }
            }

            info!("Pre-validating block");

            let validation_result = tokio::task::spawn_blocking(move || {
                prevalidate_block(
                    block_header,
                    previous_block_header,
                    partitions_guard,
                    config,
                    reward_curve,
                    ema_service_sender,
                )
                .instrument(span2)
            })
            .await
            .unwrap()
            .await;

            match validation_result {
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

                        if !matches!(status, CommitmentCacheStatus::Accepted) {
                            // Something went wrong with the commitments validation, it's time to roll back
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            let _ = commitment_cache_sender.send(
                                CommitmentCacheMessage::RollbackCommitments {
                                    commitment_txs: commitment_tx_ids,
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

                    db.update_eyre(|tx| irys_database::insert_block_header(tx, &new_block_header))
                        .unwrap();

                    let mut all_txs = submit_txs;
                    all_txs.extend_from_slice(&publish_txs);

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
                            if status != CommitmentCacheStatus::Accepted {
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

                    // WARNING: All block pre-validation needs to be completed before
                    // sending this message.
                    info!("Block is valid, sending to block tree");

                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    let _ = block_tree_sender.send(BlockTreeServiceMessage::BlockPreValidated {
                        block: new_block_header.clone(),
                        response: oneshot_tx,
                    });
                    let _ = oneshot_rx
                        .await
                        .expect("to send the BlockPreValidated message");

                    // Send the block to the gossip bus
                    tracing::trace!(
                        "sending block to bus: block height {:?}",
                        &new_block_header.height
                    );
                    if let Err(error) =
                        gossip_sender.send(GossipData::Block(new_block_header.as_ref().clone()))
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

pub async fn get_commitment_tx_in_parallel(
    commitment_tx_ids: Vec<IrysTransactionId>,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let tx_ids_clone = commitment_tx_ids.clone();

    // Set up a function to query the mempool for commitment transactions
    let mempool_future = {
        let tx_ids = tx_ids_clone.clone();
        async move {
            let (tx, rx) = oneshot::channel();
            mempool_sender.send(MempoolServiceMessage::GetCommitmentTxs {
                commitment_tx_ids: tx_ids,
                response: tx,
            })?;
            let x = rx
                .await
                .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?;
            Ok::<HashMap<IrysTransactionId, CommitmentTransaction>, eyre::Report>(x)
        }
    };

    // Set up a function to query the database for commitment transactions
    let db_future = {
        let tx_ids = commitment_tx_ids.clone();
        let db_ref = db.clone();
        async move {
            let db_tx = db_ref.tx()?;
            let mut results = HashMap::new();
            for tx_id in &tx_ids {
                if let Some(header) = commitment_tx_by_txid(&db_tx, tx_id)? {
                    results.insert(*tx_id, header);
                }
            }
            Ok::<HashMap<IrysTransactionId, CommitmentTransaction>, eyre::Report>(results)
        }
    };

    // Query mempool and database in parallel
    let (mempool_results, db_results) = tokio::join!(mempool_future, db_future);
    let mempool_map = mempool_results?;
    let db_map = db_results?;

    // Combine results, preferring mempool
    let mut headers = Vec::with_capacity(commitment_tx_ids.len());
    let mut missing = Vec::new();

    for tx_id in commitment_tx_ids {
        if let Some(header) = mempool_map.get(&tx_id) {
            headers.push(header.clone());
        } else if let Some(header) = db_map.get(&tx_id) {
            headers.push(header.clone());
        } else {
            missing.push(tx_id);
        }
    }

    if missing.is_empty() {
        Ok(headers)
    } else {
        Err(eyre::eyre!("Missing transactions: {:?}", missing))
    }
}
