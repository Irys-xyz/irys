use crate::{
    block_tree_service::BlockTreeServiceMessage,
    block_validation::{PreValidationError, prevalidate_block},
    mempool_guard::MempoolReadGuard,
    services::ServiceSenders,
};

use crate::metrics;
use async_trait::async_trait;
use futures::future::BoxFuture;
use irys_database::{
    block_header_by_hash, cached_data_root_by_data_root, commitment_tx_by_txid,
    db::IrysDatabaseExt as _, tx_header_by_txid,
};
use irys_domain::{
    BlockTreeReadGuard, CommitmentSnapshotStatus, block_index_guard::BlockIndexReadGuard,
};
use irys_reward_curve::HalvingCurve;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    BlockBody, BlockHash, CommitmentTransaction, Config, DataLedger, DataTransactionHeader,
    DatabaseProvider, H256, IrysBlockHeader, IrysTransactionId, SealedBlock, SendTraced as _,
    SystemLedger, TokioServiceHandle, Traced, get_ingress_proofs,
};
use irys_vdf::state::VdfStateReadonly;
use reth::tasks::shutdown::Shutdown;
use reth_db::Database as _;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{
    mpsc::{self, error::SendError},
    oneshot::{self, error::RecvError},
};
use tracing::{Instrument as _, debug, info, trace, warn};

#[derive(Debug, thiserror::Error)]
pub enum BlockDiscoveryError {
    #[error("Validation error: {0}")]
    BlockValidationError(PreValidationError),
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
    #[error("Invalid data ledgers length: expected {0} ledgers, got {1}")]
    InvalidDataLedgersLength(u32, usize),
    #[error("Anchor {anchor} for {item_type:?} is unknown/not part of this block's fork")]
    InvalidAnchor {
        item_type: AnchorItemType,
        anchor: BlockHash,
    },
    #[error("Invalid signature for transaction {0}")]
    InvalidSignature(IrysTransactionId),
    #[error("Transaction ID mismatch: expected {expected}, got {actual}")]
    TransactionIdMismatch {
        expected: IrysTransactionId,
        actual: IrysTransactionId,
    },
}

impl BlockDiscoveryError {
    pub(crate) fn metric_label(&self) -> &'static str {
        match self {
            Self::BlockValidationError(_) => "validation",
            Self::PreviousBlockNotFound { .. } => "previous_block_not_found",
            Self::InternalError(_) => "internal",
            Self::DuplicateTransaction(_) => "duplicate_transaction",
            Self::MissingTransactions(_) => "missing_transactions",
            Self::InvalidEpochBlock(_) => "invalid_epoch_block",
            Self::InvalidCommitmentTransaction(_) => "invalid_commitment_transaction",
            Self::InvalidDataLedgersLength(_, _) => "invalid_data_ledgers_length",
            Self::InvalidAnchor { .. } => "invalid_anchor",
            Self::InvalidSignature(_) => "invalid_signature",
            Self::TransactionIdMismatch { .. } => "transaction_id_mismatch",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AnchorItemType {
    DataTransaction { tx_id: H256 },
    IngressProof { promotion_target_id: H256, id: H256 },
    SystemTransaction { tx_id: H256 },
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
    SenderError(#[from] SendError<Traced<BlockDiscoveryMessage>>),
    #[error("Failed to receive message from the BlockDiscovery service: {0}")]
    RecvError(#[from] RecvError),
    #[error("Failed to send message to the epoch service: {0}")]
    EpochRequestFailed(String),
    #[error("Failed to send message to the block tree service: {0}")]
    BlockTreeRequestFailed(String),
}

#[async_trait::async_trait]
pub trait BlockDiscoveryFacade: Clone + Unpin + Send + Sync + 'static {
    async fn handle_block(
        &self,
        block: Arc<SealedBlock>,
        skip_vdf: bool,
    ) -> Result<(), BlockDiscoveryError>;
}

#[derive(Debug, Clone)]
pub struct BlockDiscoveryFacadeImpl {
    sender: mpsc::UnboundedSender<Traced<BlockDiscoveryMessage>>,
}

impl BlockDiscoveryFacadeImpl {
    pub fn new(sender: mpsc::UnboundedSender<Traced<BlockDiscoveryMessage>>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryFacadeImpl {
    async fn handle_block(
        &self,
        block: Arc<SealedBlock>,
        skip_vdf: bool,
    ) -> Result<(), BlockDiscoveryError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send_traced(BlockDiscoveryMessage::BlockDiscovered {
                block,
                skip_vdf,
                response: Some(tx),
            })
            .map_err(BlockDiscoveryInternalError::SenderError)?;

        rx.await.map_err(BlockDiscoveryInternalError::RecvError)?
    }
}

/// `BlockDiscoveryService` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockDiscoveryServiceInner {
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// Read only view of the block_tree
    pub block_tree_guard: BlockTreeReadGuard,
    /// Read only view of the mempool state
    pub mempool_guard: MempoolReadGuard,
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
}

#[derive(Debug)]

pub struct BlockDiscoveryService {
    shutdown: Shutdown,
    msg_rx: mpsc::UnboundedReceiver<Traced<BlockDiscoveryMessage>>,
    inner: Arc<BlockDiscoveryServiceInner>,
}

impl BlockDiscoveryService {
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_block_discovery")]
    pub fn spawn_service(
        inner: Arc<BlockDiscoveryServiceInner>,
        rx: mpsc::UnboundedReceiver<Traced<BlockDiscoveryMessage>>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning block discovery service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(
            async move {
                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner,
                };
                service
                    .start()
                    .await
                    .expect("Block discovery service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "block_discovery_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(name = "block_discovery_service_start", level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting block discovery service");

        loop {
            tokio::select! {
                biased; // enable bias so polling happens in definition order

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for block discovery service");
                    break;
                },
                // Handle commands
                traced = self.msg_rx.recv() => {
                    match traced {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            self.handle_message(msg, parent_span).await?;
                        }
                        None => {
                            warn!("Command channel closed unexpectedly");
                            break;
                        }
                    }
                },
            }
        }

        info!("Shutting down block discovery service gracefully");
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn handle_message(
        &self,
        msg: BlockDiscoveryMessage,
        parent_span: tracing::Span,
    ) -> eyre::Result<()> {
        match msg {
            BlockDiscoveryMessage::BlockDiscovered {
                block,
                skip_vdf,
                response,
            } => {
                let block_hash = block.header().block_hash;
                let block_height = block.header().height;
                let result = self.inner.clone().block_discovered(block, skip_vdf)
                    .instrument(tracing::info_span!(parent: &parent_span, "block_discovery.process", block.hash = %block_hash, block.height = block_height))
                    .await;
                if let Err(ref e) = result {
                    metrics::record_block_discovery_error(e.metric_label());
                }
                if let Some(sender) = response
                    && let Err(e) = sender.send(result)
                {
                    tracing::error!(
                        "Block discovery sender error for block {} (height {}): {:?}",
                        block_hash,
                        block_height,
                        e
                    );
                };
            }
        };

        Ok(())
    }
}

pub enum BlockDiscoveryMessage {
    BlockDiscovered {
        block: Arc<SealedBlock>,
        skip_vdf: bool,
        response: Option<oneshot::Sender<Result<(), BlockDiscoveryError>>>,
    },
}

impl BlockDiscoveryServiceInner {
    #[tracing::instrument(level = "trace", skip_all, fields(block.height = %block.header().height, block.hash = %block.header().block_hash))]
    pub async fn block_discovered(
        &self,
        block: Arc<SealedBlock>,
        skip_vdf: bool,
    ) -> Result<(), BlockDiscoveryError> {
        // Validate discovered block
        let new_block_header = block.header();
        let parent_block_hash = new_block_header.previous_block_hash;
        let transactions = block.transactions();
        let block_hash = new_block_header.block_hash;

        //====================================
        // Block header pre-validation
        //------------------------------------
        let block_tree_guard = self.block_tree_guard.clone();
        let config = self.config.clone();
        let db = self.db.clone();
        let epoch_config = self.config.consensus.epoch.clone();
        let block_tree_sender = self.service_senders.block_tree.clone();
        let blocks_in_epoch = epoch_config.num_blocks_in_epoch;

        let is_epoch_block =
            new_block_header.height > 0 && new_block_header.height.is_multiple_of(blocks_in_epoch);

        debug!(
            block.height = ?new_block_header.height,
            block.hash = %new_block_header.block_hash,
            block.global_step_counter = new_block_header.vdf_limiter_info.global_step_number,
            block.output = ?new_block_header.vdf_limiter_info.output,
            block.prev_output = ?new_block_header.vdf_limiter_info.prev_output,
            "\nPre Validating block height {} hash {} (epoch block? {})",
            new_block_header.height,
            new_block_header.block_hash,
            is_epoch_block
        );

        let gossip_sender = self.service_senders.gossip_broadcast.clone();
        let reward_curve = Arc::clone(&self.reward_curve);
        let mempool_config = self.config.consensus.mempool.clone();

        let previous_block_header = crate::block_header_lookup::get_block_header(
            &block_tree_guard,
            &db,
            parent_block_hash,
            false,
        )
        .map_err(BlockDiscoveryInternalError::DatabaseError)?
        .ok_or_else(|| BlockDiscoveryError::PreviousBlockNotFound {
            previous_block_hash: parent_block_hash,
        })?;

        //====================================
        // Submit ledger TX validation
        //------------------------------------
        // Get all the submit ledger transactions for the new block, error if not found
        // this is how we validate that the TXIDs in the Submit Ledger are real transactions.
        let publish_ledger = new_block_header
            .data_ledgers
            .get(DataLedger::Publish as usize)
            .ok_or_else(|| {
                BlockDiscoveryError::InvalidDataLedgersLength(
                    DataLedger::Publish.into(),
                    new_block_header.data_ledgers.len(),
                )
            })?;

        // Get references to transactions for anchor validation
        let submit_txs = transactions.get_ledger_txs(DataLedger::Submit);
        let publish_txs = transactions.get_ledger_txs(DataLedger::Publish);
        let commitment_txs = transactions.get_ledger_system_txs(SystemLedger::Commitment);
        let publish_tx_ids_to_check = publish_ledger.tx_ids.0.clone();

        if publish_txs.len() != publish_tx_ids_to_check.len() {
            let missing_txs = publish_tx_ids_to_check
                .into_iter()
                .filter(|id| !publish_txs.iter().any(|tx| tx.id == *id))
                .collect();
            return Err(BlockDiscoveryError::MissingTransactions(missing_txs));
        }

        if !publish_txs.is_empty() {
            // Pre-Validate the ingress-proofs for each published transaction
            for tx_header in publish_txs.iter() {
                // Get the ingress proofs for the transaction
                let tx_proofs =
                    get_ingress_proofs(publish_ledger, &tx_header.id).map_err(|_| {
                        BlockDiscoveryError::BlockValidationError(
                            PreValidationError::IngressProofsMissing,
                        )
                    })?;
                // Validate the signatures and data_roots
                for proof in tx_proofs.iter() {
                    proof.pre_validate(&tx_header.data_root).map_err(|e| {
                        BlockDiscoveryError::BlockValidationError(
                            PreValidationError::IngressProofSignatureInvalid(e.to_string()),
                        )
                    })?;

                    // Check to see if we have a confirmed data_size for the data_root
                    let cdr = self
                        .db
                        .view_eyre(|tx| cached_data_root_by_data_root(tx, tx_header.data_root))
                        .map_err(BlockDiscoveryInternalError::DatabaseError)?;

                    // If so, compare it with the data_size in the tx
                    if let Some(cdr) = cdr {
                        // If the tx data_size doesn't match the confirmed size, this is an invalid promotion
                        if cdr.data_size_confirmed && cdr.data_size != tx_header.data_size {
                            return Err(BlockDiscoveryError::BlockValidationError(
                                PreValidationError::InvalidPromotionDataSizeMismatch {
                                    txid: tx_header.id,
                                    got: tx_header.data_size,
                                    expected: cdr.data_size,
                                },
                            ));
                        }
                    }
                }
            }
        }

        info!(
            "Pre-validating block {:?} {}\ncommitments:\n{:#?}\ntransactions:\n{:?}",
            new_block_header.block_hash,
            new_block_header.height,
            new_block_header.commitment_tx_ids(),
            new_block_header.get_data_ledger_tx_ids()
        );

        //====================================
        // Anchor validation
        //------------------------------------

        // Walk the this blocks ancestors up to the anchor depth checking to see if any of the transactions
        // have already been included in a recent parent.
        let block_height = new_block_header.height;

        let anchor_expiry_depth = mempool_config.tx_anchor_expiry_depth as u64;
        let min_tx_anchor_height = block_height.saturating_sub(anchor_expiry_depth);
        let min_ingress_proof_anchor_height =
            block_height.saturating_sub(mempool_config.ingress_proof_anchor_expiry_depth.into());

        let binding = new_block_header.get_data_ledger_tx_ids();
        let incoming_data_tx_ids = binding.get(&DataLedger::Submit);

        // TODO: we can remove cloning (here and in the loop)
        // by extracting out just the metadata we need (height, hash, data ledger tx ids)
        let mut parent_block = previous_block_header.clone();

        // set of valid anchor block hashes for TRANSACTIONS
        // computed from the block tree & database
        // short vecs are probably faster than HashSet
        let mut valid_tx_anchor_blocks = vec![];
        let bt_finished_height = {
            // block for block_tree
            // explicit drop(block_tree) isn't good enough for the compiler
            let block_tree = block_tree_guard.read();
            if let Some(incoming_data_tx_ids) = incoming_data_tx_ids {
                while parent_block.height >= min_tx_anchor_height {
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
                    valid_tx_anchor_blocks.push(parent_block.block_hash);

                    if parent_block.height == 0 {
                        break;
                    }

                    // Continue the loop - get the next parent block from the block tree
                    let previous_block_header = match block_tree
                        .get_block(&parent_block.previous_block_hash)
                    {
                        Some(header) => header.clone(),
                        None =>
                        // fall back to the database
                        {
                            match db.view_eyre(|tx| {
                                block_header_by_hash(tx, &parent_block.previous_block_hash, false)
                            }) {
                                Ok(Some(header)) => header,
                                Ok(None) => {
                                    return Err(BlockDiscoveryError::PreviousBlockNotFound {
                                        previous_block_hash: parent_block.previous_block_hash,
                                    });
                                }
                                Err(e) => {
                                    return Err(BlockDiscoveryError::InternalError(
                                        BlockDiscoveryInternalError::DatabaseError(e),
                                    ));
                                }
                            }
                        }
                    };

                    parent_block = previous_block_header; // Move instead of borrow
                }
            }
            parent_block.height
        };

        let mut valid_ingress_anchor_blocks = valid_tx_anchor_blocks.clone();

        // get any remaining valid_ingress_anchor_block hashes from the block index
        // we do not need the full block headers, so we use the block index
        {
            // how many blocks do we need the block index to get to `min_ingress_proof_anchor_height`?
            let remaining = bt_finished_height.saturating_sub(min_ingress_proof_anchor_height);
            debug!(
                target = "preval-anchor",
                "min ingress proof anchor height {min_ingress_proof_anchor_height} block_height {} block_hash {} block tree finished height {bt_finished_height} remaining blocks to fetch as anchors {remaining}",
                &new_block_header.height,
                &new_block_header.block_hash
            );

            // get from the block index
            let block_index = self.block_index_guard.read();
            // this is the last block header we got from the above loop
            // we use this to ensure that
            let last_bt_safe_parent_height = parent_block.height.checked_sub(1);
            // IMPORTANT: this MUST be inclusive! matching mempool `validate_ingress_proof_anchor_for_inclusion` behaviour
            for height in min_ingress_proof_anchor_height..=bt_finished_height {
                // these block index assertions should always be true, which is why we panic (we enforce that the block tree must at least go to the boundary for migration in Config::validate)
                let block_index_item =
                    block_index
                        .get_item(height)
                        .unwrap_or_else(|| panic!("Internal critical assertion failed: Unable to get entry for height {height} from block index\nDEBUG: min ingress proof anchor height {min_ingress_proof_anchor_height} validating block: height {}, hash {} - block tree finished height {bt_finished_height} remaining blocks to fetch as anchors {remaining}", &new_block_header.height, &new_block_header.block_hash));

                if last_bt_safe_parent_height.is_some_and(|s| s == height)
                    && block_index_item.block_hash != parent_block.previous_block_hash
                {
                    // this indicates some sort of block index corruption
                    panic!(
                        "Internal critical assertion failed: block height: {} hash: {} doesn't match block_index height: {} hash: {}",
                        &parent_block.height - 1,
                        &parent_block.previous_block_hash,
                        &height,
                        &block_index_item.block_hash
                    )
                }
                valid_ingress_anchor_blocks.push(block_index_item.block_hash);
            }
        }
        trace!(
            "Valid ingress proof anchors for {}: {:?}",
            &new_block_header.block_hash, &valid_ingress_anchor_blocks
        );

        // validate anchors for submit, publish, commitments, and ingress proofs
        // (in this context, it means that anchors must be part of the fork/chain that the currently validating block is on)
        // all txs (commitment, data) use the same anchor
        // ingress proofs have a different, oftentimes longer, anchor

        // check anchors for submit ledger
        for tx in submit_txs.iter() {
            if !valid_tx_anchor_blocks.contains(&tx.anchor) {
                return Err(BlockDiscoveryError::InvalidAnchor {
                    item_type: AnchorItemType::DataTransaction { tx_id: tx.id },
                    anchor: tx.anchor,
                });
            }
        }

        // for commitments, only validate if we're not an epoch block
        // epoch blocks rollup all the commitment txs from the epoch - which means they can have anchors from anywhere in the epoch. we assume if they're in the snapshot their anchor has been validated previously.
        if !is_epoch_block {
            for tx in commitment_txs.iter() {
                if !valid_tx_anchor_blocks.contains(&tx.anchor()) {
                    return Err(BlockDiscoveryError::InvalidAnchor {
                        item_type: AnchorItemType::SystemTransaction { tx_id: tx.id() },
                        anchor: tx.anchor(),
                    });
                }
            }
        }

        // and for ingress proofs
        for tx_header in publish_txs.iter() {
            let tx_proofs = get_ingress_proofs(publish_ledger, &tx_header.id).map_err(|_| {
                BlockDiscoveryError::BlockValidationError(PreValidationError::IngressProofsMissing)
            })?;
            // Validate the anchors
            for proof in tx_proofs.iter() {
                if !valid_ingress_anchor_blocks.contains(&proof.anchor) {
                    info!(
                        "valid ingress anchor blocks: {:?},  bt_finished_height {} min_ingress_proof_anchor_height {} anchor {}, ID {}",
                        &valid_ingress_anchor_blocks,
                        &bt_finished_height,
                        &min_ingress_proof_anchor_height,
                        &proof.anchor,
                        &proof.id()
                    );
                    return Err(BlockDiscoveryError::InvalidAnchor {
                        item_type: AnchorItemType::IngressProof {
                            promotion_target_id: tx_header.id,
                            id: proof.id(),
                        },
                        anchor: proof.anchor,
                    });
                }
            }
        }

        let (parent_ema_snapshot, parent_epoch_snapshot, parent_meta) = {
            let read = block_tree_guard.read();

            let parent_block = read.blocks.get(&parent_block_hash).unwrap_or_else(|| {
                panic!(
                    "parent block {} should be in the block tree",
                    &parent_block_hash
                )
            });

            let ema_snapshot = parent_block.ema_snapshot.clone();
            // FIXME: Does this need to be for the current block if it's an epoch block?
            let epoch_snapshot = parent_block.epoch_snapshot.clone();

            (
                ema_snapshot,
                epoch_snapshot,
                (
                    parent_block.chain_state,
                    parent_block.children.clone(),
                    parent_block.timestamp,
                ),
            )
        };

        let validation_result = prevalidate_block(
            &block,
            &previous_block_header,
            parent_epoch_snapshot.clone(),
            config,
            reward_curve,
            &parent_ema_snapshot,
        )
        .in_current_span()
        .await;

        match validation_result {
            Ok(()) => {
                // Check if we've reached the end of an epoch and should finalize commitments

                let (epoch_snapshot, mut parent_commitment_snapshot) = {
                    let read = block_tree_guard.read();
                    let parent_block = read.blocks.get(&parent_block_hash).unwrap_or_else(|| {
                        panic!(
                            "Parent block {} should be in the block tree!\nDEBUG: block tree tip height: {}, child block: {} {}, parent meta:\n {:?}",
                            &parent_block_hash,
                            &read
                                .blocks
                                .get(&read.tip)
                                .expect("Tip block not found")
                                .block
                                .header()
                                .height,
                                &new_block_header.block_hash,
                                &new_block_header.height,
                                &parent_meta
                        )
                    });
                    let epoch_snapshot = parent_block.epoch_snapshot.clone();
                    let parent_commitment_snapshot =
                        parent_block.commitment_snapshot.as_ref().clone();
                    (epoch_snapshot, parent_commitment_snapshot)
                };

                if is_epoch_block {
                    let expected_commitment_tx = parent_commitment_snapshot.get_epoch_commitments();

                    // Validate epoch block has expected commitments in correct order
                    // Compare using Deref - versioned types deref to inner types
                    let commitments_match = expected_commitment_tx.iter().eq(commitment_txs.iter());
                    if !commitments_match {
                        debug!(
                            "Epoch block commitment tx for block height: {block_height} hash: {}\nexpected: {:#?}\nactual: {:#?}",
                            new_block_header.block_hash,
                            expected_commitment_tx
                                .iter()
                                .map(CommitmentTransaction::id)
                                .collect::<Vec<_>>(),
                            commitment_txs
                                .iter()
                                .map(CommitmentTransaction::id)
                                .collect::<Vec<_>>()
                        );
                        return Err(BlockDiscoveryError::InvalidEpochBlock(
                            "Epoch block commitments don't match expected".to_string(),
                        ));
                    }
                } else {
                    // Validate and add each commitment transaction for non-epoch blocks
                    for commitment_tx in commitment_txs.iter() {
                        let status = parent_commitment_snapshot
                            .get_commitment_status(commitment_tx, &epoch_snapshot);

                        // Ensure commitment is unknown (new) and from staked address
                        match status {
                            CommitmentSnapshotStatus::Accepted => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    format!(
                                        "{:?} Commitment tx {:?} included in prior block",
                                        commitment_tx.commitment_type(),
                                        commitment_tx.id()
                                    ),
                                ));
                            }
                            CommitmentSnapshotStatus::Unstaked => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    format!(
                                        "Commitment tx {} from unstaked address {:?}",
                                        commitment_tx.id(),
                                        commitment_tx.signer()
                                    ),
                                ));
                            }
                            CommitmentSnapshotStatus::InvalidPledgeCount => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Invalid pledge count in commitment transaction".to_string(),
                                ));
                            }
                            CommitmentSnapshotStatus::Unowned => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Unpledge target capacity partition not owned by signer"
                                        .to_string(),
                                ));
                            }
                            CommitmentSnapshotStatus::UnpledgePending => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Duplicate unpledge for the same capacity partition in snapshot".to_string(),
                                ));
                            }
                            CommitmentSnapshotStatus::UnstakePending => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Unstake already pending for signer".to_string(),
                                ));
                            }
                            CommitmentSnapshotStatus::HasActivePledges => {
                                return Err(BlockDiscoveryError::InvalidCommitmentTransaction(
                                    "Unstake not allowed while pledges are active".to_string(),
                                ));
                            }
                            CommitmentSnapshotStatus::Unknown => {} // Success case
                        }

                        // Add commitment and validate it's accepted
                        let add_status = parent_commitment_snapshot
                            .add_commitment(commitment_tx, &epoch_snapshot);
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
                let header_for_broadcast = Arc::clone(new_block_header);
                block_tree_sender
                    .send_traced(BlockTreeServiceMessage::BlockPreValidated {
                        block,
                        skip_vdf_validation: skip_vdf,
                        response: oneshot_tx,
                    })
                    .map_err(|channel_error| {
                        BlockDiscoveryInternalError::BlockTreeRequestFailed(format!(
                            "Failed to send BlockPreValidated message: {}",
                            channel_error
                        ))
                    })?;

                oneshot_rx
                    .await
                    .map_err(|e| {
                        BlockDiscoveryInternalError::BlockTreeRequestFailed(format!(
                            "Failed to receive response for BlockPreValidated: {}",
                            e
                        ))
                    })?
                    .map_err(BlockDiscoveryError::BlockValidationError)?;

                // Send the block to the gossip bus
                tracing::trace!("sending block to bus: block height {:?}", &block_height);
                if let Err(error) =
                    gossip_sender.send_traced(GossipBroadcastMessageV2::from(header_for_broadcast))
                {
                    tracing::error!(
                        "Failed to send gossip message for block {} (height {}): {}",
                        block_hash,
                        block_height,
                        error
                    );
                }

                Ok(())
            }
            Err(err) => {
                tracing::error!(
                    "Block validation error for block {} (height {}): {:?}",
                    block_hash,
                    block_height,
                    err
                );
                Err(BlockDiscoveryError::BlockValidationError(err))
            }
        }
    }
}

/// Query database for commitment transactions by IDs
async fn query_commitment_txs_from_db(
    commitment_tx_ids: &[IrysTransactionId],
    db: &DatabaseProvider,
) -> eyre::Result<HashMap<IrysTransactionId, CommitmentTransaction>> {
    let db_ref = db.clone();
    let tx_ids = commitment_tx_ids.to_vec();

    tokio::task::spawn_blocking(move || {
        let db_tx = db_ref.tx()?;
        let mut results = HashMap::new();
        for tx_id in &tx_ids {
            if let Some(header) = commitment_tx_by_txid(&db_tx, tx_id)? {
                results.insert(*tx_id, header);
            }
        }
        Ok::<HashMap<IrysTransactionId, CommitmentTransaction>, eyre::Report>(results)
    })
    .await
    .map_err(|e| eyre::eyre!("Database task join error: {}", e))?
}

/// Merge commitment transactions from mempool and database, returning ordered vec
fn merge_commitment_tx_results(
    commitment_tx_ids: &[IrysTransactionId],
    mempool_map: HashMap<IrysTransactionId, CommitmentTransaction>,
    db_map: HashMap<IrysTransactionId, CommitmentTransaction>,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let mut headers = Vec::with_capacity(commitment_tx_ids.len());
    let mut missing = Vec::new();

    // Combine results, preferring mempool over database
    for tx_id in commitment_tx_ids {
        if let Some(header) = mempool_map.get(tx_id) {
            headers.push(header.clone());
        } else if let Some(header) = db_map.get(tx_id) {
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

/// Get all commitment transactions from the mempool and database using direct read guard access.
pub async fn get_commitment_tx_in_parallel(
    commitment_tx_ids: &[IrysTransactionId],
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let mempool_future = {
        let guard = mempool_guard.clone();
        let tx_ids = commitment_tx_ids.to_vec();
        async move {
            let mempool_map = guard.get_commitment_txs(&tx_ids).await;
            Ok::<HashMap<IrysTransactionId, CommitmentTransaction>, eyre::Report>(mempool_map)
        }
    };

    let (mempool_result, db_result) = tokio::join!(
        mempool_future,
        query_commitment_txs_from_db(commitment_tx_ids, db)
    );

    merge_commitment_tx_results(commitment_tx_ids, mempool_result?, db_result?)
}

/// Get all data transactions from the mempool and database using direct read guard access.
pub async fn get_data_tx_in_parallel(
    data_tx_ids: Vec<IrysTransactionId>,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<DataTransactionHeader>> {
    let guard = mempool_guard.clone();

    let get_data_txs = move |tx_ids: Vec<IrysTransactionId>| -> BoxFuture<
        'static,
        eyre::Result<Vec<Option<DataTransactionHeader>>>,
    > {
        let guard = guard.clone();
        Box::pin(async move {
            let mempool_map = guard.get_data_txs(&tx_ids).await;
            let results: Vec<Option<DataTransactionHeader>> = tx_ids
                .iter()
                .map(|id| mempool_map.get(id).cloned())
                .collect();
            Ok(results)
        })
    };

    get_data_tx_in_parallel_inner(data_tx_ids, get_data_txs, db).await
}

pub async fn build_block_body_for_processed_block_header(
    block_header: &IrysBlockHeader,
    mempool_read_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BlockBody> {
    let data_transaction_ids = block_header
        .data_ledgers
        .iter()
        .flat_map(|data_ledger| &data_ledger.tx_ids.0)
        .copied()
        .collect::<Vec<_>>();
    let commitment_transaction_ids = block_header
        .system_ledgers
        .iter()
        .flat_map(|commitment_ledger| &commitment_ledger.tx_ids.0)
        .copied()
        .collect::<Vec<_>>();

    let (data_txs, commitment_txs) = tokio::join!(
        get_data_tx_in_parallel(data_transaction_ids, mempool_read_guard, db),
        get_commitment_tx_in_parallel(&commitment_transaction_ids, mempool_read_guard, db)
    );
    let data_txs = data_txs?;
    let commitment_txs = commitment_txs?;

    let block_body = BlockBody {
        block_hash: block_header.block_hash,
        data_transactions: data_txs,
        commitment_transactions: commitment_txs,
    };

    Ok(block_body)
}

/// Get all data transactions from the mempool and database
/// with a custom get_data_txs function (this is used by the mempool)
#[tracing::instrument(level = "trace", skip_all, fields(tx.count = data_tx_ids.len()))]
pub async fn get_data_tx_in_parallel_inner<F>(
    data_tx_ids: Vec<IrysTransactionId>,
    get_data_txs: F,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<DataTransactionHeader>>
where
    F: Fn(
        Vec<IrysTransactionId>,
    ) -> BoxFuture<'static, eyre::Result<Vec<Option<DataTransactionHeader>>>>,
{
    let tx_ids_clone = data_tx_ids.clone();

    // Set up a function to query the mempool for data transactions
    let mempool_future = {
        let tx_ids = tx_ids_clone.clone();
        async move {
            let results = get_data_txs(tx_ids.clone())
                .await
                .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?;

            let x: HashMap<IrysTransactionId, DataTransactionHeader> = tx_ids
                .into_iter()
                .zip(results.into_iter())
                .fold(HashMap::new(), |mut map, (id, opt)| {
                    if let Some(header) = opt {
                        map.insert(id, header);
                    }
                    map
                });

            Ok::<HashMap<IrysTransactionId, DataTransactionHeader>, eyre::Report>(x)
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
            Ok::<HashMap<IrysTransactionId, DataTransactionHeader>, eyre::Report>(results)
        }
    };

    // Query mempool and database in parallel
    let (mempool_results, db_results) = tokio::join!(mempool_future, db_future);

    let mempool_map = mempool_results?;
    let db_map = db_results?;

    debug!(
        mempool_count = mempool_map.len(),
        db_count = db_map.len(),
        "Query results retrieved"
    );
    trace!(
        mempool_keys = ?mempool_map.keys().collect::<Vec<_>>(),
        db_keys = ?db_map.keys().collect::<Vec<_>>(),
        "Detailed query results"
    );

    // Combine results, preferring mempool
    // this is because unmigrated promoted txs get their promoted_height updated in the mempool ONLY
    // so we need to prefer it.
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
