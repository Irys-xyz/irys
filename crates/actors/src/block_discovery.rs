use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeReadGuard, BlockTreeServiceMessage},
    block_validation::prevalidate_block,
    epoch_service::PartitionAssignmentsReadGuard,
    mempool_service::MempoolServiceMessage,
    services::ServiceSenders,
};
use actix::prelude::*;
use async_trait::async_trait;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _, tx_header_by_txid,
    CommitmentSnapshotStatus, SystemLedger,
};
use irys_reward_curve::HalvingCurve;
use irys_types::{
    BlockHash, CommitmentTransaction, Config, DataLedger, DatabaseProvider, GossipBroadcastMessage,
    IrysBlockHeader, IrysTransactionHeader, IrysTransactionId,
};
use irys_vdf::state::VdfStateReadonly;
use reth_db::Database as _;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc::UnboundedSender, oneshot},
    time::timeout,
};
use tracing::{debug, error, info, Instrument as _, Span};

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryActor {
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// Read only view of the block_tree
    pub block_tree_guard: BlockTreeReadGuard,
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

#[derive(Debug, thiserror::Error)]
pub enum BlockDiscoveryError {
    #[error("Validation error: {0}")]
    BlockValidationError(eyre::Report),
    #[error("Failed to get previous block header. Previous block hash: {previous_block_hash:?}")]
    PreviousBlockNotFound {
        /// The hash of the previous block that was not found
        previous_block_hash: BlockHash,
    },
    #[error("{0}")]
    InternalError(BlockDiscoveryInternalError),
    #[error("Duplicate data transaction detected: {0}")]
    DuplicateTransaction(IrysTransactionId),
    #[error("Missing transactions: {0:?}")]
    MissingTransactions(Vec<IrysTransactionId>),
    #[error("Invalid epoch block: {0}")]
    InvalidEpochBlock(String),
    #[error("Invalid commitment transaction: {0}")]
    InvalidCommitmentTransaction(String),
}

impl From<BlockDiscoveryInternalError> for BlockDiscoveryError {
    fn from(err: BlockDiscoveryInternalError) -> Self {
        Self::InternalError(err)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlockDiscoveryInternalError {
    #[error("Failed to communicate with the mempool: {0}")]
    MempoolRequestFailed(String),
    #[error("Database error: {0:?}")]
    DatabaseError(eyre::Report),
    #[error("Failed to send message to the BlockDiscovery service: {0}")]
    MailboxError(#[from] actix::MailboxError),
    #[error("Failed to send message to the epoch service: {0}")]
    EpochRequestFailed(String),
}

#[async_trait::async_trait]
pub trait BlockDiscoveryFacade: Clone + Unpin + Send + Sync + 'static {
    async fn handle_block(&self, block: IrysBlockHeader) -> Result<(), BlockDiscoveryError>;
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
    async fn handle_block(&self, block: IrysBlockHeader) -> Result<(), BlockDiscoveryError> {
        self.addr
            .send(BlockDiscoveredMessage(Arc::new(block)))
            .await
            .map_err(BlockDiscoveryInternalError::MailboxError)?
    }
}
/// When a block is discovered, either produced locally or received from
/// a network peer, this message is broadcast.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockDiscoveryError>")]
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
    type Result = ResponseFuture<Result<(), BlockDiscoveryError>>;

    fn handle(&mut self, msg: BlockDiscoveredMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let span = self.span.clone();
        let _span = span.enter();
        // Validate discovered block
        let new_block_header = msg.0;
        let prev_block_hash = new_block_header.previous_block_hash;

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_index_guard = self.block_index_guard.clone();
        let block_tree_guard = self.block_tree_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let ema_service_sender = self.service_senders.ema.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();
        let epoch_config = self.config.consensus.epoch.clone();
        let span2 = self.span.clone();
        let block_tree_sender = self.service_senders.block_tree.clone();
        let mempool_sender = self.service_senders.mempool.clone();
        let epoch_sender = self.service_senders.epoch_service.clone();

        debug!(height = ?new_block_header.height,
            global_step_counter = ?new_block_header.vdf_limiter_info.global_step_number,
            output = ?new_block_header.vdf_limiter_info.output,
            prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "\nPre Validating block"
        );

        let gossip_sender = self.service_senders.gossip_broadcast.clone();
        let reward_curve = Arc::clone(&self.reward_curve);
        let mempool = self.service_senders.mempool.clone();
        let mempool_config = self.config.consensus.mempool.clone();
        Box::pin(async move {
            let span3 = span2.clone();
            let _span = span3.enter();

            let previous_block_header = {
                let (tx_prev, rx_prev) = oneshot::channel();
                mempool_sender
                    .send(MempoolServiceMessage::GetBlockHeader(
                        prev_block_hash,
                        false,
                        tx_prev,
                    ))
                    .map_err(|channel_error| {
                        BlockDiscoveryInternalError::MempoolRequestFailed(channel_error.to_string())
                    })?;
                match rx_prev
                    .await
                    .map_err(|e| BlockDiscoveryInternalError::MempoolRequestFailed(e.to_string()))?
                {
                    Some(hdr) => hdr,
                    None => db
                        .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash, false))
                        .map_err(BlockDiscoveryInternalError::DatabaseError)?
                        .ok_or_else(|| BlockDiscoveryError::PreviousBlockNotFound {
                            previous_block_hash: prev_block_hash,
                        })?,
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

            let submit_tx_ids_to_check = new_block_header.data_ledgers[DataLedger::Submit]
                .tx_ids
                .0
                .clone();

            let (tx, rx) = oneshot::channel();
            mempool
                .send(MempoolServiceMessage::GetDataTxs(
                    submit_tx_ids_to_check.clone(),
                    tx,
                ))
                .map_err(|channel_error| {
                    BlockDiscoveryInternalError::MempoolRequestFailed(channel_error.to_string())
                })?;

            let submit_txs = rx
                .await
                .map_err(|e| {
                    BlockDiscoveryInternalError::MempoolRequestFailed(format!(
                        "Mempool response error: {}",
                        e
                    ))
                })?
                .into_iter()
                .flatten()
                .collect::<Vec<IrysTransactionHeader>>();

            if submit_txs.len() != submit_tx_ids_to_check.len() {
                return Err(BlockDiscoveryError::MissingTransactions(
                    submit_tx_ids_to_check
                        .into_iter()
                        .filter(|id| !submit_txs.iter().any(|tx| tx.id == *id))
                        .collect(),
                ));
            }

            //====================================
            // Publish ledger TX Validation
            //------------------------------------
            // 1. Validate the proof
            // 2. Validate the transaction
            // 3. Update the local tx headers index to include the ingress-proof.
            //    This keeps the transaction from getting re-promoted each block.
            //    (this last step performed in mempool after the block is confirmed)

            let publish_tx_ids_to_check = new_block_header.data_ledgers[DataLedger::Publish]
                .tx_ids
                .0
                .clone();

            let (tx, rx) = oneshot::channel();
            mempool
                .send(MempoolServiceMessage::GetDataTxs(
                    publish_tx_ids_to_check.clone(),
                    tx,
                ))
                .map_err(|channel_error| {
                    BlockDiscoveryInternalError::MempoolRequestFailed(channel_error.to_string())
                })?;

            let publish_txs = rx
                .await
                .map_err(|e| {
                    BlockDiscoveryInternalError::MempoolRequestFailed(format!(
                        "Mempool response error: {}",
                        e
                    ))
                })?
                .into_iter()
                .flatten()
                .collect::<Vec<IrysTransactionHeader>>();

            if publish_txs.len() != publish_tx_ids_to_check.len() {
                let missing_txs = publish_tx_ids_to_check
                    .into_iter()
                    .filter(|id| !publish_txs.iter().any(|tx| tx.id == *id))
                    .collect();
                return Err(BlockDiscoveryError::MissingTransactions(missing_txs));
            }

            if !publish_txs.is_empty() {
                let publish_proofs =
                    match &new_block_header.data_ledgers[DataLedger::Publish].proofs {
                        Some(proofs) => proofs,
                        None => {
                            return Err(BlockDiscoveryError::BlockValidationError(eyre::eyre!(
                                "Ingress proofs missing"
                            )));
                        }
                    };

                // Pre-Validate the ingress-proof by verifying the signature
                for (i, tx_header) in publish_txs.iter().enumerate() {
                    if let Err(e) = publish_proofs.0[i].pre_validate(&tx_header.data_root) {
                        return Err(BlockDiscoveryError::BlockValidationError(eyre::eyre!(
                            "Invalid ingress proof signature: {}",
                            e
                        )));
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

            // Validate commitments transactions exist (if there are commitment txids in the block)
            let mut commitments: Vec<CommitmentTransaction> = Vec::new();
            if let Some(commitment_ledger) = commitment_ledger {
                debug!(
                    "incoming block commitment txids, height {}\n{:#?}",
                    new_block_header.height, commitment_ledger
                );
                match get_commitment_tx_in_parallel(
                    commitment_ledger.tx_ids.0.clone(),
                    &mempool_sender,
                    &db,
                )
                .await
                {
                    Ok(tx) => {
                        commitments = tx;
                    }
                    Err(e) => {
                        error!("Failed to collect commitment transactions: {:?}", e);
                        return Err(BlockDiscoveryError::MissingTransactions(
                            commitment_ledger.tx_ids.0.clone(),
                        ));
                    }
                }
            }

            info!(
                "Pre-validating block {:?} {}\ncommitments:\n{:#?}\ntransactions:\n{:?}",
                new_block_header.block_hash,
                new_block_header.height,
                new_block_header.get_commitment_ledger_tx_ids(),
                commitments.iter().map(|x| x.id).collect::<Vec<_>>()
            );

            // Walk the this blocks ancestors up to the anchor depth checking to see if any of the transactions
            // have already been included in a recent parent.
            let block_height = new_block_header.height;

            let anchor_expiry_depth = mempool_config.anchor_expiry_depth as u64;
            let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);
            let mut parent_block = previous_block_header.clone();

            let binding = new_block_header.get_data_ledger_tx_ids();
            let incoming_data_tx_ids = binding.get(&DataLedger::Submit);

            if let Some(incoming_data_tx_ids) = incoming_data_tx_ids {
                while parent_block.height >= min_anchor_height {
                    // Check to see if any data txids appeared in prior blocks
                    let parent_data_tx_ids = parent_block.get_data_ledger_tx_ids();

                    // Compare each ledger type between current and parent blocks
                    if let Some(parent_txids) = parent_data_tx_ids.get(&DataLedger::Submit) {
                        // Check for intersection between current and parent txids for this ledger
                        for txid in incoming_data_tx_ids {
                            if parent_txids.contains(txid) {
                                return Err(BlockDiscoveryError::DuplicateTransaction(*txid));
                            }
                        }
                    }

                    if parent_block.height == 0 {
                        break;
                    }

                    // Continue the loop - get the next parent block
                    // Get the next parent block and own it
                    let previous_block_header = match db.view_eyre(|tx| {
                        block_header_by_hash(tx, &parent_block.previous_block_hash, false)
                    }) {
                        Ok(Some(header)) => header,
                        Ok(None) => break,
                        Err(e) => {
                            return Err(BlockDiscoveryError::InternalError(
                                BlockDiscoveryInternalError::DatabaseError(e),
                            ))
                        }
                    };

                    parent_block = previous_block_header; // Move instead of borrow
                }
            }

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
                Ok(()) => {
                    // add block to mempool
                    mempool_sender
                        .send(MempoolServiceMessage::IngestBlocks {
                            prevalidated_blocks: vec![new_block_header.clone()],
                        })
                        .map_err(|channel_error| {
                            BlockDiscoveryInternalError::MempoolRequestFailed(
                                channel_error.to_string(),
                            )
                        })?;

                    // all txs
                    let mut all_txs = submit_txs;
                    all_txs.extend_from_slice(&publish_txs);

                    // Check if we've reached the end of an epoch and should finalize commitments
                    let blocks_in_epoch = epoch_config.num_blocks_in_epoch;
                    let is_epoch_block = block_height > 0 && block_height % blocks_in_epoch == 0;

                    let arc_commitment_txs = Arc::new(commitments);

                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    epoch_sender
                        .send(crate::EpochServiceMessage::GetCommitmentStateGuard(
                            oneshot_tx,
                        ))
                        .map_err(|e| {
                            BlockDiscoveryInternalError::EpochRequestFailed(format!(
                                "Failed to send GetCommitmentStateGuard message: {}",
                                e
                            ))
                        })?;

                    let commitment_state_guard = oneshot_rx.await.unwrap();

                    // Get the current epoch snapshot from the parent block
                    let mut parent_snapshot = block_tree_guard
                        .read()
                        .get_commitment_snapshot(&prev_block_hash)
                        .expect("parent block to be in block_tree")
                        .as_ref()
                        .clone();

                    if is_epoch_block {
                        let expected_commitment_tx = parent_snapshot.get_epoch_commitments();

                        // Validate epoch block has expected commitments in correct order
                        let commitments_match =
                            expected_commitment_tx.iter().eq(arc_commitment_txs.iter());
                        if !commitments_match {
                            debug!(
                                "Epoch block commitment tx for block height: {block_height}\nexpected: {:#?}\nactual: {:#?}",
                                expected_commitment_tx.iter().map(|x| x.id).collect::<Vec<_>>(),
                                arc_commitment_txs.iter().map(|x| x.id).collect::<Vec<_>>()
                            );
                            return Err(BlockDiscoveryError::InvalidEpochBlock(
                                "Epoch block commitments don't match expected".to_string(),
                            ));
                        }

                        // Send NewEpochMessage with previous and current epoch blocks
                        let block_item = block_index_guard
                            .read()
                            .get_item(block_height - blocks_in_epoch)
                            .expect("previous epoch block to be in block index")
                            .clone();
                        let previous_epoch_block = db
                            .view(|tx| block_header_by_hash(tx, &block_item.block_hash, false))
                            .map_err(|e| {
                                BlockDiscoveryInternalError::DatabaseError(eyre::eyre!(
                                    "Database error retrieving previous epoch block: {:?}",
                                    e
                                ))
                            })?
                            .map_err(|report| {
                                BlockDiscoveryError::BlockValidationError(eyre::eyre!(
                                    "Previous epoch block not found in database: {}",
                                    report
                                ))
                            })?;

                        let (oneshot_tx, rx) = tokio::sync::oneshot::channel();
                        epoch_sender
                            .send(crate::EpochServiceMessage::NewEpoch {
                                new_epoch_block: new_block_header.clone(),
                                previous_epoch_block,
                                commitments: arc_commitment_txs.clone(),
                                sender: oneshot_tx,
                            })
                            .map_err(|e| {
                                BlockDiscoveryInternalError::EpochRequestFailed(format!(
                                    "Failed to send NewEpoch message: {}",
                                    e
                                ))
                            })?;
                        let _ = rx.await.map_err(|e| {
                            BlockDiscoveryInternalError::EpochRequestFailed(format!(
                                "Failed to receive response for NewEpoch: {}",
                                e
                            ))
                        })?;
                    } else {
                        // Validate and add each commitment transaction for non-epoch blocks
                        for commitment_tx in arc_commitment_txs.iter() {
                            let is_staked = commitment_state_guard.is_staked(commitment_tx.signer);
                            let status =
                                parent_snapshot.get_commitment_status(commitment_tx, is_staked);

                            // Ensure commitment is unknown (new) and from staked address
                            match status {
                                CommitmentSnapshotStatus::Accepted => {
                                    return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                        "Commitment tx included in prior block".to_string(),
                                    ));
                                }
                                CommitmentSnapshotStatus::Unsupported => {
                                    return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                        "Commitment tx of unsupported type".to_string(),
                                    ));
                                }
                                CommitmentSnapshotStatus::Unstaked => {
                                    return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                        format!(
                                            "Commitment tx {} from unstaked address {:?}",
                                            commitment_tx.id, commitment_tx.signer
                                        ),
                                    ));
                                }
                                CommitmentSnapshotStatus::Unknown => {} // Success case
                            }

                            // Add commitment and validate it's accepted
                            let is_staked_in_current_epoch =
                                commitment_state_guard.is_staked(commitment_tx.signer);
                            let add_status = parent_snapshot
                                .add_commitment(commitment_tx, is_staked_in_current_epoch);
                            if add_status != CommitmentSnapshotStatus::Accepted {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Commitment tx is invalid".to_string(),
                                ));
                            }
                        }
                    }

                    // WARNING: All block pre-validation needs to be completed before
                    // sending this message.
                    info!("Block is valid, sending to block tree");

                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    block_tree_sender
                        .send(BlockTreeServiceMessage::BlockPreValidated {
                            block: new_block_header.clone(),
                            commitment_txs: arc_commitment_txs,
                            response: oneshot_tx,
                        })
                        .map_err(|channel_error| {
                            BlockDiscoveryInternalError::MempoolRequestFailed(format!(
                                "Failed to send BlockPreValidated message: {}",
                                channel_error
                            ))
                        })?;

                    oneshot_rx
                        .await
                        .map_err(|e| {
                            BlockDiscoveryInternalError::MempoolRequestFailed(format!(
                                "Failed to receive response for BlockPreValidated: {}",
                                e
                            ))
                        })?
                        .map_err(|e| {
                            BlockDiscoveryError::BlockValidationError(eyre::eyre!(
                                "Block pre-validation failed: {}",
                                e
                            ))
                        })?;

                    // Send the block to the gossip bus
                    tracing::trace!(
                        "sending block to bus: block height {:?}",
                        &new_block_header.height
                    );
                    if let Err(error) = gossip_sender.send(GossipBroadcastMessage::from(
                        new_block_header.as_ref().clone(),
                    )) {
                        tracing::error!("Failed to send gossip message: {}", error);
                    }

                    Ok(())
                }
                Err(err) => {
                    tracing::error!("Block validation error {:?}", err);
                    Err(BlockDiscoveryError::BlockValidationError(err))
                }
            }
        })
    }
}

/// Get all commitment transactions from the mempool and database
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

            match mempool_sender.send(MempoolServiceMessage::GetCommitmentTxs {
                commitment_tx_ids: tx_ids,
                response: tx,
            }) {
                Ok(()) => {
                    // Message was sent successfully, wait for response with timeout
                    let result = timeout(Duration::from_secs(5), rx)
                    .await
                    .map_err(|_| eyre::eyre!("Mempool request timed out after 5 seconds - service may be unresponsive"))?
                    .map_err(|e| eyre::eyre!("Mempool response channel closed: {}", e))?;

                    Ok(result)
                }
                Err(_) => {
                    // Channel is closed - either no receiver was ever created or it was dropped
                    Err(eyre::eyre!("Mempool service is not available (channel closed - service may not be running)"))
                }
            }
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

/// Get all data transactions from the mempool and database
pub async fn get_data_tx_in_parallel(
    data_tx_ids: Vec<IrysTransactionId>,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<IrysTransactionHeader>> {
    let tx_ids_clone = data_tx_ids.clone();

    // Set up a function to query the mempool for data transactions
    let mempool_future = {
        let tx_ids = tx_ids_clone.clone();
        async move {
            let (tx, rx) = oneshot::channel();
            mempool_sender.send(MempoolServiceMessage::GetDataTxs(tx_ids, tx))?;
            let x = rx
                .await
                .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?
                .into_iter()
                .filter(Option::is_some)
                .map(|v| (v.clone().unwrap().id, v.unwrap()))
                .collect::<HashMap<IrysTransactionId, IrysTransactionHeader>>();
            Ok::<HashMap<IrysTransactionId, IrysTransactionHeader>, eyre::Report>(x)
        }
    };

    // Set up a function to query the database for commitment transactions
    let db_future = {
        let tx_ids = data_tx_ids.clone();
        let db_ref = db.clone();
        async move {
            let db_tx = db_ref.tx()?;
            let mut results = HashMap::new();
            for tx_id in &tx_ids {
                if let Some(header) = tx_header_by_txid(&db_tx, tx_id)? {
                    results.insert(*tx_id, header);
                }
            }
            Ok::<HashMap<IrysTransactionId, IrysTransactionHeader>, eyre::Report>(results)
        }
    };

    // Query mempool and database in parallel
    let (mempool_results, db_results) = tokio::join!(mempool_future, db_future);

    let mempool_map = mempool_results?;
    let db_map = db_results?;

    debug!(
        "mempool_results:\n {:?}",
        mempool_map.iter().map(|x| x.0).collect::<Vec<_>>()
    );
    debug!(
        "db_results:\n {:?}",
        db_map.iter().map(|x| x.0).collect::<Vec<_>>()
    );

    // Combine results, preferring mempool
    let mut headers = Vec::with_capacity(data_tx_ids.len());
    let mut missing = Vec::new();

    for tx_id in data_tx_ids {
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
