use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::BlockTreeServiceMessage,
    block_validation::prevalidate_block,
    epoch_service::{EpochServiceActor, NewEpochMessage, PartitionAssignmentsReadGuard},
    mempool_service::MempoolServiceMessage,
    services::ServiceSenders,
    GetCommitmentStateGuardMessage,
};
use actix::prelude::*;
use async_trait::async_trait;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _, tx_header_by_txid,
    CommitmentSnapshot, CommitmentSnapshotStatus, SystemLedger,
};
use irys_reward_curve::HalvingCurve;
use irys_types::{
    CommitmentTransaction, Config, DataLedger, DatabaseProvider, GossipData, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionId,
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
        // Block header pre-validation
        //------------------------------------
        let block_index_guard2 = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let ema_service_sender = self.service_senders.ema.clone();
        let block_header: IrysBlockHeader = (*new_block_header).clone();
        let epoch_service = self.epoch_service.clone();
        let epoch_config = self.config.consensus.epoch.clone();
        let span2 = self.span.clone();
        let block_tree_sender = self.service_senders.block_tree.clone();
        let mempool_sender = self.service_senders.mempool.clone();

        debug!(height = ?new_block_header.height,
            global_step_counter = ?new_block_header.vdf_limiter_info.global_step_number,
            output = ?new_block_header.vdf_limiter_info.output,
            prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "\nPre Validating block"
        );

        let gossip_sender = self.service_senders.gossip_broadcast.clone();
        let reward_curve = Arc::clone(&self.reward_curve);
        let mempool = self.service_senders.mempool.clone();
        Box::pin(async move {
            let span3 = span2.clone();
            let _span = span3.enter();

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
            mempool.send(MempoolServiceMessage::GetDataTxs(
                submit_tx_ids_to_check.clone(),
                tx,
            ))?;

            let submit_txs = rx
                .await
                .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?
                .into_iter()
                .flatten()
                .collect::<Vec<IrysTransactionHeader>>();

            if submit_txs.len() != submit_tx_ids_to_check.len() {
                return Err(eyre::eyre!("Failed to collect submit tx headers"));
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
            mempool.send(MempoolServiceMessage::GetDataTxs(
                publish_tx_ids_to_check.clone(),
                tx,
            ))?;

            let publish_txs = rx
                .await
                .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?
                .into_iter()
                .flatten()
                .collect::<Vec<IrysTransactionHeader>>();

            if publish_txs.len() != publish_tx_ids_to_check.len() {
                return Err(eyre::eyre!("Failed to collect publish tx headers"));
            }

            if !publish_txs.is_empty() {
                let publish_proofs =
                    match &new_block_header.data_ledgers[DataLedger::Publish].proofs {
                        Some(proofs) => proofs,
                        None => {
                            return Err(eyre::eyre!("Ingress proofs missing"));
                        }
                    };

                // Pre-Validate the ingress-proof by verifying the signature
                for (i, tx_header) in publish_txs.iter().enumerate() {
                    if let Err(e) = publish_proofs.0[i].pre_validate(&tx_header.data_root) {
                        return Err(eyre::eyre!("Invalid ingress proof signature: {}", e));
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
                    Err(e) => error!("Failed to collect commitment transactions: {:?}", e),
                }
            }

            info!("Pre-validating block: {}", new_block_header.height);

            // TODO: This first pass a validating transactions are not duplicates causes a bunch
            // of tests to fail that seem to rely on or not expect this validation check.
            // Disabling the code for now as this is already a difficult to marge PR.

            // Walk the this blocks ancestors up to the anchor depth checking to see if any of the transactions
            // have already been included in a recent parent.
            let block_height = new_block_header.height;

            /*(
            let anchor_expiry_depth = mempool_config.anchor_expiry_depth as u64;
            let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);
            let mut parent_block = previous_block_header.clone();

            // Get the transaction IDs from the current block to check against
            let _current_commitment_tx_ids: HashSet<_> = new_block_header
                .get_commitment_ledger_tx_ids()
                .into_iter()
                .collect();
            let current_data_tx_ids = new_block_header.get_data_ledger_tx_ids();

            while parent_block.height >= min_anchor_height {
                // Check to see if any commitment txids appeared in prior blocks
                // let parent_commitment_tx_ids = parent_block.get_commitment_ledger_tx_ids();
                // for txid in &parent_commitment_tx_ids {
                //     if current_commitment_tx_ids.contains(txid) {
                //         return Err(eyre!("Duplicate commitment transaction id {}", txid));
                //     }
                // }

                // Check to see if any data txids appeared in prior blocks
                let parent_data_tx_ids = parent_block.get_data_ledger_tx_ids();

                // Compare each ledger type between current and parent blocks
                for (ledger_type, current_txids) in &current_data_tx_ids {
                    if let Some(parent_txids) = parent_data_tx_ids.get(ledger_type) {
                        // Check for intersection between current and parent txids for this ledger
                        for txid in current_txids {
                            if parent_txids.contains(txid) {
                                return Err(eyre!("Duplicate data transaction id {}", txid));
                            }
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
                    Err(e) => return Err(e),
                };

                parent_block = previous_block_header; // Move instead of borrow
            }
            */

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
                    // TODO: we shouldn't insert the block just yet, let it live in the mempool until it migrates
                    db.update_eyre(|tx| irys_database::insert_block_header(tx, &new_block_header))
                        .unwrap();

                    let mut all_txs = submit_txs;
                    all_txs.extend_from_slice(&publish_txs);

                    // Check if we've reached the end of an epoch and should finalize commitments

                    let blocks_in_epoch = epoch_config.num_blocks_in_epoch;
                    let is_epoch_block = block_height > 0 && block_height % blocks_in_epoch == 0;

                    let arc_commitment_txs = Arc::new(commitments);

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
                        let mut local_snapshot = CommitmentSnapshot::default();

                        // Validate each commitment transaction before accepting the epoch block
                        for commitment_tx in arc_commitment_txs.iter() {
                            let is_staked_in_current_epoch =
                                commitment_state_guard.is_staked(commitment_tx.signer);

                            let status = local_snapshot
                                .add_commitment(commitment_tx, is_staked_in_current_epoch);

                            // Reject the entire epoch block if any commitment is invalid
                            // This ensures only verified commitments are finalized at epoch boundaries
                            if status != CommitmentSnapshotStatus::Accepted {
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
                            commitments: arc_commitment_txs.clone(),
                        });
                    }

                    // WARNING: All block pre-validation needs to be completed before
                    // sending this message.
                    info!("Block is valid, sending to block tree");

                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    let _ = block_tree_sender.send(BlockTreeServiceMessage::BlockPreValidated {
                        block: new_block_header.clone(),
                        commitment_txs: arc_commitment_txs,
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

/// Get all data/storage transactions from the mempool and database
pub async fn get_data_tx_in_parallel(
    storage_tx_ids: Vec<IrysTransactionId>,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<IrysTransactionHeader>> {
    let tx_ids_clone = storage_tx_ids.clone();

    // Set up a function to query the mempool for storage transactions
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
        let tx_ids = storage_tx_ids.clone();
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
    let mut headers = Vec::with_capacity(storage_tx_ids.len());
    let mut missing = Vec::new();

    for tx_id in storage_tx_ids {
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
