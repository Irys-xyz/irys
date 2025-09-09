pub mod chunks;
pub mod commitment_txs;
pub mod data_txs;
pub mod facade;
pub mod ingress_proofs;
pub mod lifecycle;
pub mod pledge_provider;

pub use chunks::*;
pub use facade::*;

use crate::block_discovery::get_data_tx_in_parallel_inner;
use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::block_validation::calculate_perm_storage_total_fee;
use crate::pledge_provider::MempoolPledgeProvider;
use crate::services::ServiceSenders;
use crate::shadow_tx_generator::PublishLedgerWithTxs;
use eyre::{eyre, OptionExt as _};
use futures::future::BoxFuture;
use futures::FutureExt as _;
use irys_database::tables::IngressProofs;
use irys_database::{cached_data_root_by_data_root, ingress_proofs_by_data_root, SystemLedger};
use irys_domain::{
    get_atomic_file, BlockTreeReadGuard, CommitmentSnapshotStatus, StorageModulesReadGuard,
};
use irys_primitives::CommitmentType;
use irys_reth_node_bridge::{ext::IrysRethRpcTestContextExt as _, IrysRethNodeAdapter};
use irys_storage::RecoveredMempoolState;
use irys_types::ingress::IngressProof;
use irys_types::{
    app_state::DatabaseProvider, Config, IrysBlockHeader, IrysTransactionCommon, IrysTransactionId,
    H256, U256,
};
use irys_types::{
    storage_pricing::{
        calculate_term_fee,
        phantoms::{Irys, NetworkFee},
        Amount,
    },
    Address, Base64, ChunkPathHash, CommitmentTransaction, CommitmentValidationError, DataRoot,
    DataTransactionHeader, MempoolConfig, TxChunkOffset, UnpackedChunk,
};
use irys_types::{IngressProofsList, TokioServiceHandle};
use lru::LruCache;
use reth::rpc::types::BlockId;
use reth::tasks::shutdown::Shutdown;
use reth::tasks::TaskExecutor;
use reth_db::cursor::*;
use reth_db::{Database as _, DatabaseError};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::broadcast;
use tokio::sync::{mpsc::UnboundedReceiver, oneshot, RwLock};
use tracing::{debug, error, info, instrument, trace, warn, Instrument as _};

#[derive(Debug)]
pub struct Inner {
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub config: Config,
    /// `task_exec` is used to spawn background jobs on reth's MT tokio runtime
    /// instead of the actor executor runtime, while also providing some `QoL`
    pub exec: TaskExecutor,
    pub irys_db: DatabaseProvider,
    pub reth_node_adapter: IrysRethNodeAdapter,
    pub mempool_state: AtomicMempoolState,
    /// Reference to all the services we can send messages to
    pub service_senders: ServiceSenders,
    pub storage_modules_guard: StorageModulesReadGuard,
    /// Pledge provider for commitment transaction validation
    pub pledge_provider: MempoolPledgeProvider,
}

/// Messages that the Mempool Service handler supports
#[derive(Debug)]
pub enum MempoolServiceMessage {
    /// Block Confirmed, read publish txs from block. Overwrite copies in mempool with proof
    BlockConfirmed(Arc<IrysBlockHeader>),
    /// Ingress Chunk, Add to CachedChunks, generate_ingress_proof, gossip chunk
    IngestChunk(
        UnpackedChunk,
        oneshot::Sender<Result<(), ChunkIngressError>>,
    ),
    IngestIngressProof(IngressProof, oneshot::Sender<Result<(), IngressProofError>>),
    /// Ingress Pre-validated Block
    IngestBlocks {
        prevalidated_blocks: Vec<Arc<IrysBlockHeader>>,
    },
    /// Confirm commitment tx exists in mempool
    CommitmentTxExists(H256, oneshot::Sender<Result<bool, TxReadError>>),
    /// Ingress CommitmentTransaction into the mempool
    ///
    /// This function performs a series of checks and validations:
    /// - Skips the transaction if it is already known to be invalid or previously processed
    /// - Validates the transaction's anchor and signature
    /// - Inserts the valid transaction into the mempool and database
    /// - Processes any pending pledge transactions that depended on this commitment
    /// - Gossips the transaction to peers if accepted
    /// - Caches the transaction for unstaked signers to be reprocessed later
    IngestCommitmentTx(
        CommitmentTransaction,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Confirm data tx exists in mempool or database
    DataTxExists(H256, oneshot::Sender<Result<bool, TxReadError>>),
    /// validate and process an incoming DataTransactionHeader
    IngestDataTx(
        DataTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Return filtered list of candidate txns
    /// Filtering based on funding status etc based on the provided EVM block ID
    /// If `None` is provided, the latest canonical block is used
    GetBestMempoolTxs(Option<BlockId>, oneshot::Sender<eyre::Result<MempoolTxs>>),
    /// Retrieves a list of CommitmentTransactions based on the provided tx ids
    GetCommitmentTxs {
        commitment_tx_ids: Vec<IrysTransactionId>,
        response: oneshot::Sender<HashMap<IrysTransactionId, CommitmentTransaction>>,
    },
    /// Get DataTransactionHeader from mempool or mdbx
    GetDataTxs(
        Vec<IrysTransactionId>,
        oneshot::Sender<Vec<Option<DataTransactionHeader>>>,
    ),
    /// Get block header from the mempool cache
    GetBlockHeader(H256, bool, oneshot::Sender<Option<IrysBlockHeader>>),
    InsertPoAChunk(H256, Base64, oneshot::Sender<()>),
    GetState(oneshot::Sender<AtomicMempoolState>),
    /// Remove the set of txids from any blocklists (recent_invalid_txs)
    RemoveFromBlacklist(Vec<H256>, oneshot::Sender<()>),
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    /// handle inbound MempoolServiceMessage and send oneshot responses where required to do so
    pub fn handle_message<'a>(
        &'a mut self,
        msg: MempoolServiceMessage,
    ) -> BoxFuture<'a, eyre::Result<()>> {
        Box::pin(async move {
            match msg {
                MempoolServiceMessage::GetDataTxs(txs, response) => {
                    let response_message = self.handle_get_data_tx_message(txs).await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::BlockConfirmed(block) => {
                    let _unused_response_message = self.handle_block_confirmed_message(block).await;
                }
                MempoolServiceMessage::IngestBlocks {
                    prevalidated_blocks,
                } => {
                    let _unused_response_message = self
                        .handle_ingress_blocks_message(prevalidated_blocks)
                        .await;
                }
                MempoolServiceMessage::IngestCommitmentTx(commitment_tx, response) => {
                    let response_message = self
                        .handle_ingress_commitment_tx_message(commitment_tx)
                        .await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::IngestChunk(chunk, response) => {
                    let response_value: Result<(), ChunkIngressError> =
                        self.handle_chunk_ingress_message(chunk).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetBestMempoolTxs(block_id, response) => {
                    let response_value = self.handle_get_best_mempool_txs(block_id).await;
                    // Return selected transactions grouped by type
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetCommitmentTxs {
                    commitment_tx_ids,
                    response,
                } => {
                    let response_value = self
                        .handle_get_commitment_tx_message(commitment_tx_ids)
                        .await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::DataTxExists(txid, response) => {
                    let response_value = self.handle_data_tx_exists_message(txid).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetBlockHeader(hash, include_chunk, response) => {
                    let response_value = self
                        .handle_get_block_header_message(hash, include_chunk)
                        .await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::CommitmentTxExists(txid, response) => {
                    let response_value = self.handle_commitment_tx_exists_message(txid).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::IngestDataTx(tx, response) => {
                    let response_value = self.handle_data_tx_ingress_message(tx).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::InsertPoAChunk(block_hash, chunk_data, response) => {
                    self.mempool_state
                        .write()
                        .await
                        .prevalidated_blocks_poa
                        .insert(block_hash, chunk_data);
                    if let Err(e) = response.send(()) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetState(response) => {
                    let _ = response
                        .send(Arc::clone(&self.mempool_state))
                        .inspect_err(|e| tracing::error!("response.send() error: {:?}", e));
                }
                MempoolServiceMessage::IngestIngressProof(ingress_proof, response) => {
                    let response_value = self.handle_ingest_ingress_proof(ingress_proof);
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::RemoveFromBlacklist(tx_ids, response) => {
                    let response_value = self.remove_from_blacklists(tx_ids).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
            }
            Ok(())
        })
    }

    async fn remove_from_blacklists(&mut self, tx_ids: Vec<H256>) {
        let mut state = self.mempool_state.write().await;
        for tx_id in tx_ids {
            state.recent_invalid_tx.pop(&tx_id);
        }
    }

    #[instrument(skip_all)]
    pub async fn validate_anchor_for_inclusion(
        &self,
        min_anchor_height: u64,
        max_anchor_height: u64,
        tx: &impl IrysTransactionCommon,
    ) -> eyre::Result<bool> {
        let tx_id = tx.id();
        let anchor = tx.anchor();
        let anchor_height = self.get_anchor_height(tx_id, anchor).await?;

        // these have to be inclusive so we handle txs near height 0 correctly
        let new_enough = anchor_height >= min_anchor_height;
        let old_enough = anchor_height <= max_anchor_height;
        if old_enough && new_enough {
            Ok(true)
        } else if !old_enough {
            warn!("Tx {tx_id} anchor {anchor} has height {anchor_height}, which is too new compared to max height {max_anchor_height}");
            Ok(false)
        } else if !new_enough {
            warn!("Tx {tx_id} anchor {anchor} has height {anchor_height}, which is too old compared to min height {min_anchor_height}");
            Ok(false)
        } else {
            eyre::bail!("SHOULDNT HAPPEN: {tx_id} anchor {anchor} has height {anchor_height}, min: {min_anchor_height}, max: {max_anchor_height}");
        }
    }

    #[instrument(skip(self), fields(parent_block_id = ?parent_evm_block_id), err)]
    async fn handle_get_best_mempool_txs(
        &mut self,
        parent_evm_block_id: Option<BlockId>,
    ) -> eyre::Result<MempoolTxs> {
        let mempool_state = &self.mempool_state;
        let mut fees_spent_per_address: HashMap<Address, U256> = HashMap::new();
        let mut confirmed_commitments = HashSet::new();
        let mut commitment_tx = Vec::new();
        let mut unfunded_address = HashSet::new();

        let max_commitments: usize = self
            .config
            .node_config
            .consensus_config()
            .mempool
            .max_commitment_txs_per_block
            .try_into()
            .expect("max_commitment_txs_per_block to fit into usize");

        let current_height = self.get_latest_block_height()?;
        let min_anchor_height = current_height.saturating_sub(
            (self.config.consensus.mempool.anchor_expiry_depth as u64)
                .saturating_sub(self.config.consensus.block_migration_depth as u64),
        );

        let max_anchor_height =
            current_height.saturating_sub(self.config.consensus.block_migration_depth as u64);

        debug!("Anchor bounds for inclusion @ {current_height}: >= {min_anchor_height}, <= {max_anchor_height}");

        // Helper function that verifies transaction funding and tracks cumulative fees
        // Returns true if the transaction can be funded based on current account balance
        // and previously included transactions in this block
        let mut check_funding = |tx: &dyn IrysTransactionCommon| -> bool {
            let signer = tx.signer();

            // Skip transactions from addresses with previously unfunded transactions
            // This ensures we don't include any transactions (including pledges) from
            // addresses that couldn't afford their stake commitments
            if unfunded_address.contains(&signer) {
                return false;
            }

            let fee = tx.total_cost();
            let current_spent = *fees_spent_per_address.get(&signer).unwrap_or(&U256::zero());

            // Calculate total required balance including previously selected transactions

            // get balance state for the block we're building off of
            let balance: U256 = self
                .reth_node_adapter
                .rpc
                .get_balance_irys(signer, parent_evm_block_id);

            let has_funds = balance >= current_spent + fee;

            // Track fees for this address regardless of whether this specific transaction is included
            fees_spent_per_address
                .entry(signer)
                .and_modify(|val| *val += fee)
                .or_insert(fee);

            // If transaction cannot be funded, mark the entire address as unfunded
            // Since stakes are processed before pledges, this prevents inclusion of
            // pledge commitments when their associated stake commitment is unfunded
            if !has_funds {
                debug!(
                    signer = ?signer,
                    balance = ?balance,
                    "Transaction funding check failed"
                );
                unfunded_address.insert(signer);
                return false;
            }

            has_funds
        };

        // Get all necessary snapshots and canonical chain info in a single read operation
        let (canonical, last_block, commitment_snapshot, epoch_snapshot, ema_snapshot) = {
            let tree = self.block_tree_read_guard.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block = canonical
                .last()
                .ok_or_eyre("Empty canonical chain")?
                .clone();

            // Get all snapshots for the tip block
            let ema_snapshot = tree
                .get_ema_snapshot(&last_block.block_hash)
                .ok_or_else(|| eyre!("EMA snapshot not found for tip block"))?;
            let epoch_snapshot = tree
                .get_epoch_snapshot(&last_block.block_hash)
                .ok_or_else(|| eyre!("Epoch snapshot not found for tip block"))?;
            let commitment_snapshot = tree
                .get_commitment_snapshot(&last_block.block_hash)
                .map_err(|e| eyre!("Failed to get commitment snapshot: {}", e))?;

            (
                canonical,
                last_block,
                commitment_snapshot,
                epoch_snapshot,
                ema_snapshot,
            )
        };

        info!(
            head_height = last_block.height,
            block_hash = ?last_block.block_hash,
            chain_length = canonical.len(),
            "Starting mempool transaction selection"
        );

        for entry in canonical.iter() {
            let commitment_tx_ids = entry.system_ledgers.get(&SystemLedger::Commitment);
            if let Some(commitment_tx_ids) = commitment_tx_ids {
                for tx_id in &commitment_tx_ids.0 {
                    confirmed_commitments.insert(*tx_id);
                }
            }
        }

        // Process commitments in the mempool in priority order
        let mempool_state_guard = mempool_state.read().await;

        // Collect all stake and pledge commitments from mempool
        let mut sorted_commitments = mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| {
                txs.iter()
                    .filter(|tx| {
                        matches!(
                            tx.commitment_type,
                            CommitmentType::Stake | CommitmentType::Pledge { .. }
                        )
                    })
                    .cloned()
            })
            .collect::<Vec<_>>();

        // Sort all commitments according to our priority rules
        sorted_commitments.sort();

        // Process sorted commitments
        // create a throw away commitment snapshot so we can simulate behaviour before including a commitment tx in returned txs
        let mut simulation_commitment_snapshot = commitment_snapshot.as_ref().clone();
        for tx in &sorted_commitments {
            if confirmed_commitments.contains(&tx.id) {
                debug!(
                    tx_id = ?tx.id,
                    commitment_type = ?tx.commitment_type,
                    signer = ?tx.signer,
                    "Skipping already confirmed commitment transaction"
                );
                continue;
            }

            // Check funding before simulation
            if !check_funding(tx) {
                continue;
            }

            if !self
                .validate_anchor_for_inclusion(min_anchor_height, max_anchor_height, tx)
                .await?
            {
                continue;
            }

            // signer stake status check
            if matches!(tx.commitment_type, CommitmentType::Stake) {
                let is_staked = epoch_snapshot.is_staked(tx.signer);
                debug!(
                    tx_id = ?tx.id,
                    signer = ?tx.signer,
                    is_staked = is_staked,
                    "Checking stake status for commitment tx"
                );
                if is_staked {
                    // if a signer has stake commitments in the mempool, but is already staked, we should ignore them
                    continue;
                }
            }
            // simulation check
            {
                let simulation = simulation_commitment_snapshot.add_commitment(tx, &epoch_snapshot);

                // skip commitments that would not be accepted
                if simulation != CommitmentSnapshotStatus::Accepted {
                    warn!(
                        commitment_type = ?tx.commitment_type,
                        tx_id = ?tx.id,
                        simulation_status = ?simulation,
                        "Commitment tx rejected by simulation"
                    );
                    continue;
                }
            }

            debug!(
                tx_id = ?tx.id,
                commitment_type = ?tx.commitment_type,
                signer = ?tx.signer,
                fee = ?tx.total_cost(),
                selected_count = commitment_tx.len() + 1,
                max_commitments,
                "Adding commitment transaction to block"
            );
            commitment_tx.push(tx.clone());

            // if we have reached the maximum allowed number of commitment txs per block
            // do not push anymore
            if commitment_tx.len() >= max_commitments {
                break;
            }
        }
        drop(mempool_state_guard);

        // Log commitment selection summary
        if !commitment_tx.is_empty() {
            let commitment_summary =
                commitment_tx
                    .iter()
                    .fold((0_usize, 0_usize), |(stakes, pledges), tx| {
                        match tx.commitment_type {
                            CommitmentType::Stake => (stakes + 1, pledges),
                            CommitmentType::Pledge { .. } => (stakes, pledges + 1),
                            _ => (stakes, pledges),
                        }
                    });
            info!(
                selected_commitments = commitment_tx.len(),
                stake_txs = commitment_summary.0,
                pledge_txs = commitment_summary.1,
                max_allowed = max_commitments,
                "Completed commitment transaction selection"
            );
        }

        // Prepare data transactions for inclusion after commitments
        let mut submit_ledger_txs = self.get_pending_submit_ledger_txs().await;
        let total_data_available = submit_ledger_txs.len();

        // Sort data transactions by fee (highest first) to maximize revenue
        // The miner will get proportionally higher rewards for higher term fee values
        submit_ledger_txs.sort_by(|a, b| match b.user_fee().cmp(&a.user_fee()) {
            std::cmp::Ordering::Equal => a.id.cmp(&b.id),
            fee_ordering => fee_ordering,
        });

        // Apply block size constraint and funding checks to data transactions
        let mut submit_tx = Vec::new();
        let max_data_txs: usize = self
            .config
            .node_config
            .consensus_config()
            .mempool
            .max_data_txs_per_block
            .try_into()
            .expect("max_data_txs_per_block to fit into usize");

        // Select data transactions in fee-priority order, respecting funding limits
        // and maximum transaction count per block
        for tx in submit_ledger_txs {
            // Validate fees based on ledger type
            let Ok(ledger) = irys_types::DataLedger::try_from(tx.ledger_id) else {
                trace!(
                    tx_id = ?tx.id,
                    ledger_id = tx.ledger_id,
                    "Skipping tx: invalid ledger ID"
                );
                continue;
            };
            match ledger {
                irys_types::DataLedger::Publish => {
                    // For Publish ledger, validate both term and perm fees
                    // Calculate expected fees based on current EMA
                    let Ok(expected_term_fee) =
                        self.calculate_term_storage_fee(tx.data_size, &ema_snapshot)
                    else {
                        continue;
                    };

                    let Ok(expected_perm_fee) = self.calculate_perm_storage_fee(
                        tx.data_size,
                        expected_term_fee,
                        &ema_snapshot,
                    ) else {
                        continue;
                    };

                    // Validate term fee
                    if tx.term_fee < expected_term_fee {
                        trace!(
                            tx_id = ?tx.id,
                            actual_term_fee = ?tx.term_fee,
                            expected_term_fee = ?expected_term_fee,
                            "Skipping Publish tx: insufficient term_fee"
                        );
                        continue;
                    }

                    // Validate perm fee must be present for Publish ledger
                    let Some(perm_fee) = tx.perm_fee else {
                        // Missing perm_fee for Publish ledger transaction is invalid
                        warn!(
                            tx_id = ?tx.id,
                            signer = ?tx.signer,
                            "Invalid Publish tx: missing perm_fee"
                        );
                        // todo: add to list of invalid txs because all publish txs must have perm fee present
                        continue;
                    };
                    if perm_fee < expected_perm_fee.amount {
                        trace!(
                            tx_id = ?tx.id,
                            actual_perm_fee = ?perm_fee,
                            expected_perm_fee = ?expected_perm_fee.amount,
                            "Skipping Publish tx: insufficient perm_fee"
                        );
                        continue;
                    }
                }
                irys_types::DataLedger::Submit => {
                    // todo: add to list of invalid txs because we don't support Submit txs
                }
            }

            if !self
                .validate_anchor_for_inclusion(min_anchor_height, max_anchor_height, &tx)
                .await?
            {
                continue;
            }

            trace!(
                tx_id = ?tx.id,
                signer = ?tx.signer(),
                fee = ?tx.total_cost(),
                "Checking funding for data transaction"
            );
            if check_funding(&tx) {
                trace!(
                    tx_id = ?tx.id,
                    signer = ?tx.signer(),
                    fee = ?tx.total_cost(),
                    selected_count = submit_tx.len() + 1,
                    max_data_txs,
                    "Data transaction passed funding check"
                );
                submit_tx.push(tx);
                if submit_tx.len() >= max_data_txs {
                    break;
                }
            } else {
                trace!(
                    tx_id = ?tx.id,
                    signer = ?tx.signer(),
                    fee = ?tx.total_cost(),
                    reason = "insufficient_funds",
                    "Data transaction failed funding check"
                );
            }
        }

        // note: publish txs are sorted internally by the get_publish_txs_and_proofs fn
        let publish_txs_and_proofs = self.get_publish_txs_and_proofs().await?;

        // Calculate total fees and log final summary
        let total_fee_collected: U256 = submit_tx
            .iter()
            .map(irys_types::IrysTransactionCommon::user_fee)
            .fold(U256::zero(), irys_types::U256::saturating_add)
            .saturating_add(
                commitment_tx
                    .iter()
                    .map(irys_types::IrysTransactionCommon::total_cost)
                    .fold(U256::zero(), irys_types::U256::saturating_add),
            );

        info!(
            commitment_txs = commitment_tx.len(),
            data_txs = submit_tx.len(),
            publish_txs = publish_txs_and_proofs.txs.len(),
            total_fee_collected = ?total_fee_collected,
            unfunded_addresses = unfunded_address.len(),
            "Mempool transaction selection completed"
        );

        // Check for high rejection rate
        let total_commitments_available = sorted_commitments.len();
        let total_available = total_commitments_available + total_data_available;
        let total_selected = commitment_tx.len() + submit_tx.len();

        if total_available > 0 {
            const REJECTION_RATE_THRESHOLD: usize = 70;
            let rejection_rate = ((total_available - total_selected) * 100) / total_available;
            if rejection_rate > REJECTION_RATE_THRESHOLD {
                warn!(
                    rejection_rate = rejection_rate,
                    total_available,
                    total_selected,
                    commitments_available = total_commitments_available,
                    commitments_selected = commitment_tx.len(),
                    data_available = total_data_available,
                    data_selected = submit_tx.len(),
                    unfunded_addresses = unfunded_address.len(),
                    "High transaction rejection rate detected"
                );
            }
        }

        // Return selected transactions grouped by type
        Ok(MempoolTxs {
            commitment_tx,
            submit_tx,
            publish_tx: publish_txs_and_proofs,
        })
    }

    pub async fn get_publish_txs_and_proofs(&self) -> Result<PublishLedgerWithTxs, eyre::Error> {
        let mut publish_txs: Vec<DataTransactionHeader> = Vec::new();
        let mut publish_proofs: Vec<IngressProof> = Vec::new();

        {
            let read_tx = self
                .irys_db
                .tx()
                .map_err(|e| eyre!("Failed to create DB transaction: {}", e))?;

            let mut read_cursor = read_tx
                .new_cursor::<IngressProofs>()
                .map_err(|e| eyre!("Failed to create DB read cursor: {}", e))?;

            let walker = read_cursor
                .walk(None)
                .map_err(|e| eyre!("Failed to create DB read cursor walker: {}", e))?;

            let ingress_proofs = walker
                .collect::<Result<HashMap<_, _>, _>>()
                .map_err(|e| eyre!("Failed to collect ingress proofs from database: {}", e))?;

            let mut publish_txids: Vec<H256> = Vec::new();

            // Loop tough all the data_roots with ingress proofs and find corresponding transaction ids
            for data_root in ingress_proofs.keys() {
                let cached_data_root = cached_data_root_by_data_root(&read_tx, *data_root).unwrap();
                if let Some(cached_data_root) = cached_data_root {
                    let txids = cached_data_root.txid_set;
                    debug!(tx_ids = ?txids, "Publish candidates");
                    publish_txids.extend(txids)
                }
            }

            // Loop though all the pending tx to see which haven't been promoted
            let txs = self.handle_get_data_tx_message(publish_txids.clone()).await;
            // TODO: improve this
            let mut tx_headers = get_data_tx_in_parallel_inner(
                publish_txids,
                |_tx_ids| {
                    {
                        let txs = txs.clone(); // whyyyy
                        async move { Ok(txs) }
                    }
                    .boxed()
                },
                &self.irys_db,
            )
            .await
            .unwrap_or(vec![]);

            // so the resulting publish_txs & proofs are sorted
            tx_headers.sort_by(|a, b| a.id.cmp(&b.id));

            for tx_header in &tx_headers {
                debug!(
                    "Processing candidate tx {} {:#?}",
                    &tx_header.id, &tx_header
                );
                let is_promoted = tx_header.promoted_height.is_some();

                if is_promoted {
                    // If it's promoted skip it
                    warn!(
                        "Publish candidate {} is already promoted? {}",
                        &tx_header.id, &is_promoted
                    );
                } else {
                    // If it's not promoted, validate the proofs

                    // Get the proofs for this tx
                    let proofs = ingress_proofs_by_data_root(&read_tx, tx_header.data_root)?;

                    let mut tx_proofs = Vec::new();

                    // Check for the correct number of ingress proofs
                    if (proofs.len() as u64) < self.config.consensus.number_of_ingress_proofs_total
                    {
                        // Not enough ingress proofs to promote this tx
                        info!(
                            "Not promoting tx {} - insufficient proofs (got {} wanted {})",
                            &tx_header.id,
                            &proofs.len(),
                            self.config.consensus.number_of_ingress_proofs_total
                        );
                        continue;
                    } else {
                        // Collect enough ingress proofs for promotion, but no more
                        for i in 0..self.config.consensus.number_of_ingress_proofs_total {
                            tx_proofs.push(proofs[i as usize].1.proof.clone());
                        }

                        // Update the lists for the publish ledger txid and tx_proofs share an index
                        publish_txs.push(tx_header.clone());
                        publish_proofs.append(&mut tx_proofs);
                    }
                }
            }
        }

        let txs = &publish_txs.iter().map(|h| h.id).collect::<Vec<_>>();
        debug!(?txs, "Publish transactions");

        debug!("Processing Publish transactions {:#?}", &publish_txs);

        Ok(PublishLedgerWithTxs {
            txs: publish_txs,
            proofs: if publish_proofs.is_empty() {
                None
            } else {
                Some(IngressProofsList::from(publish_proofs))
            },
        })
    }

    /// return block header from mempool, if found
    pub async fn handle_get_block_header_message(
        &self,
        block_hash: H256,
        include_chunk: bool,
    ) -> Option<IrysBlockHeader> {
        let guard = self.mempool_state.read().await;

        //read block from mempool
        let mut block = guard.prevalidated_blocks.get(&block_hash).cloned();

        // retrieve poa from mempool and include in returned block
        if include_chunk {
            if let Some(ref mut b) = block {
                b.poa.chunk = guard.prevalidated_blocks_poa.get(&block_hash).cloned();
            }
        }

        block
    }

    // Resolves an anchor (block hash) to it's height
    pub async fn get_anchor_height(
        &self,
        tx_id: IrysTransactionId,
        anchor: H256,
    ) -> Result<u64, TxIngressError> {
        // check the mempool, then block tree, then DB
        Ok(
            if let Some(height) = {
                let guard = self.mempool_state.read().await;
                guard.prevalidated_blocks.get(&anchor).map(|h| h.height)
            } {
                height
            } else if let Some(height) = {
                // in a block so rust doesn't complain about it being held across an await point
                // I suspect if let Some desugars to something that lint doesn't like
                let guard = self.block_tree_read_guard.read();
                guard.get_block(&anchor).map(|h| h.height)
            } {
                height
            } else if let Some(hdr) = {
                let read_tx = self.read_tx().map_err(|_| TxIngressError::DatabaseError)?;
                irys_database::block_header_by_hash(&read_tx, &anchor, false)
                    .map_err(|_| TxIngressError::DatabaseError)?
            } {
                hdr.height
            } else {
                Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
                return Err(TxIngressError::InvalidAnchor);
            },
        )
    }

    // Helper to validate anchor
    // this takes in an IrysTransaction and validates the anchor
    // if the anchor is valid, returns the tx back with the height that made the anchor canonical (i.e the block height)
    #[instrument(skip_all, fields(tx_id = %tx.id(), anchor = %tx.anchor()))]
    pub async fn validate_anchor(
        &mut self,
        tx: &impl IrysTransactionCommon,
    ) -> Result<u64, TxIngressError> {
        let tx_id = tx.id();
        let anchor = tx.anchor();

        let latest_height = self.get_latest_block_height()?;

        let anchor_height = self.get_anchor_height(tx_id, anchor).await?;

        // is this anchor too old?

        let min_anchor_height =
            latest_height.saturating_sub(self.config.consensus.mempool.anchor_expiry_depth as u64);

        let too_old = anchor_height < min_anchor_height;

        if !too_old {
            debug!("valid block hash anchor for tx ");
            return Ok(anchor_height);
        } else {
            Self::mark_tx_as_invalid(
                self.mempool_state.write().await,
                tx_id,
                format!(
                    "Invalid anchor value for tx {tx_id} - anchor {anchor}@{anchor_height} is too old ({anchor_height}<{min_anchor_height}"
                ),
            );

            return Err(TxIngressError::InvalidAnchor);
        }
    }

    /// ingest a block into the mempool
    async fn handle_ingress_blocks_message(&self, prevalidated_blocks: Vec<Arc<IrysBlockHeader>>) {
        let mut mempool_state_guard = self.mempool_state.write().await;
        for block in prevalidated_blocks {
            // insert poa into mempool
            if let Some(chunk) = &block.poa.chunk {
                mempool_state_guard
                    .prevalidated_blocks_poa
                    .insert(block.block_hash, chunk.clone());
            };

            // insert block into mempool without poa
            let mut block_without_chunk = (*block).clone();
            block_without_chunk.poa.chunk = None;
            mempool_state_guard
                .prevalidated_blocks
                .insert(block.block_hash, (block_without_chunk).clone());
        }
    }

    pub async fn persist_mempool_to_disk(&self) -> eyre::Result<()> {
        let base_path = self.config.node_config.mempool_dir();

        let commitment_tx_path = base_path.join("commitment_tx");
        fs::create_dir_all(commitment_tx_path.clone())
            .expect("to create the mempool/commitment_tx dir");
        let commitment_hash_map = self.get_all_commitment_tx().await;
        for tx in commitment_hash_map.values() {
            // Create a filepath for this transaction
            let tx_path = commitment_tx_path.join(format!("{}.json", tx.id));

            // Check to see if the file exists
            if tx_path.exists() {
                continue;
            }

            // If not, write it to  {mempool_dir}/commitment_tx/{txid}.json
            let json = serde_json::to_string(tx).unwrap();
            debug!("{}", json);
            debug!("{}", tx_path.to_str().unwrap());

            let mut file = get_atomic_file(tx_path).unwrap();
            file.write_all(json.as_bytes())?;
            file.commit()?;
        }

        let storage_tx_path = base_path.join("storage_tx");
        fs::create_dir_all(storage_tx_path.clone()).expect("to create the mempool/storage_tx dir");
        let storage_hash_map = self.get_all_storage_tx().await;
        for tx in storage_hash_map.values() {
            // Create a filepath for this transaction
            let tx_path = storage_tx_path.join(format!("{}.json", tx.id));

            // Check to see if the file exists
            if tx_path.exists() {
                continue;
            }

            // If not, write it to  {mempool_dir}/storage_tx/{txid}.json
            let json = serde_json::to_string(tx).unwrap();

            let mut file = get_atomic_file(tx_path).unwrap();
            file.write_all(json.as_bytes())?;
            file.commit()?;
        }

        Ok(())
    }

    pub async fn restore_mempool_from_disk(&mut self) {
        let recovered =
            RecoveredMempoolState::load_from_disk(&self.config.node_config.mempool_dir(), true)
                .await;

        for (_txid, commitment_tx) in recovered.commitment_txs {
            let _ = self
                .handle_ingress_commitment_tx_message(commitment_tx)
                .await
                .inspect_err(|_| {
                    tracing::warn!("Commitment tx ingress error during mempool restore from disk")
                });
        }

        for (_txid, storage_tx) in recovered.storage_txs {
            let _ = self
                .handle_data_tx_ingress_message(storage_tx)
                .await
                .inspect_err(|_| {
                    tracing::warn!("Storage tx ingress error during mempool restore from disk")
                });
        }

        self.wipe_blacklists().await;
    }

    // wipes all the "blacklists", primarily used after trying to restore the mempool from disk so that validation errors then (i.e if we have a saved tx that uses an anchor from some blocks that we forgot we when restarted) don't affect block validation
    // right now this only wipes `recent_invalid_tx`
    pub async fn wipe_blacklists(&mut self) {
        let mut write = self.mempool_state.write().await;
        write.recent_invalid_tx.clear();
    }

    /// Helper that opens a read-only database transaction from the Irys mempool state.
    ///
    /// Returns a `Tx<RO>` handle if successful, or a `ChunkIngressError::DatabaseError`
    /// if the transaction could not be created. Logs an error if the transaction fails.
    pub fn read_tx(
        &self,
    ) -> Result<irys_database::reth_db::mdbx::tx::Tx<reth_db::mdbx::RO>, DatabaseError> {
        self.irys_db
            .tx()
            .inspect_err(|e| error!("database error reading tx: {:?}", e))
    }

    // Helper to verify signature
    #[instrument(skip_all, fields(tx_id = %tx.id()))]
    pub async fn validate_signature<T: IrysTransactionCommon>(
        &mut self,
        tx: &T,
    ) -> Result<(), TxIngressError> {
        if tx.is_signature_valid() {
            info!("Tx {} signature is valid", &tx.id());
            Ok(())
        } else {
            let mempool_state = &self.mempool_state;

            // TODO: we need to use the hash of the *entire* tx struct (including ID and signature)
            // to prevent malformed txs from poisoning legitimate transactions

            // re-derive the tx_id to ensure we don't get poisoned
            // let tx_id = H256::from(alloy_primitives::keccak256(tx.signature().as_bytes()).0);

            mempool_state
                .write()
                .await
                .recent_invalid_tx
                .put(tx.id(), ());
            warn!("Tx {} signature is invalid", &tx.id());
            Err(TxIngressError::InvalidSignature)
        }
    }

    /// Marks a given tx as invalid, adding it's ID to `recent_invalid_tx` and removing it from `recent_valid_tx`
    pub fn mark_tx_as_invalid(
        mut state: tokio::sync::RwLockWriteGuard<'_, MempoolState>,
        tx_id: IrysTransactionId,
        err_reason: impl ToString,
    ) {
        warn!("Tx {} is invalid: {:?}", &tx_id, &err_reason.to_string());
        // let mut state: tokio::sync::RwLockWriteGuard<'_, MempoolState> = self.mempool_state.write().await;
        state.recent_invalid_tx.put(tx_id, ());
        state.recent_valid_tx.pop(&tx_id);
    }

    // Helper to get the canonical chain and latest height
    fn get_latest_block_height(&self) -> Result<u64, TxIngressError> {
        let canon_chain = self.block_tree_read_guard.read().get_canonical_chain();
        let latest = canon_chain.0.last().ok_or(TxIngressError::Other(
            "unable to get canonical chain from block tree".to_owned(),
        ))?;

        Ok(latest.height)
    }

    /// Calculate the expected protocol fee for permanent storage
    /// This includes base network fee + ingress proof rewards
    #[tracing::instrument(err)]
    pub fn calculate_perm_storage_fee(
        &self,
        bytes_to_store: u64,
        term_fee: U256,
        ema: &Arc<irys_domain::EmaSnapshot>,
    ) -> Result<Amount<(NetworkFee, Irys)>, TxIngressError> {
        // Calculate total perm fee including ingress proof rewards
        let total_perm_fee =
            calculate_perm_storage_total_fee(bytes_to_store, term_fee, ema, &self.config)
                .map_err(TxIngressError::other_display)?;

        Ok(total_perm_fee)
    }

    /// Calculate the expected term fee for temporary storage
    /// This matches the calculation in the pricing API and uses dynamic epoch count
    pub fn calculate_term_storage_fee(
        &self,
        bytes_to_store: u64,
        ema: &Arc<irys_domain::EmaSnapshot>,
    ) -> Result<U256, TxIngressError> {
        // Get the latest block height to calculate next block's expires
        let latest_height = self.get_latest_block_height()?;
        let next_block_height = latest_height + 1;

        // Calculate expires for the next block using the shared utility
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            next_block_height,
            self.config.consensus.epoch.num_blocks_in_epoch,
            self.config.consensus.epoch.submit_ledger_epoch_length,
        );

        // Calculate term fee using the storage pricing module
        calculate_term_fee(
            bytes_to_store,
            epochs_for_storage,
            &self.config.consensus,
            ema.ema_for_public_pricing(),
        )
        .map_err(|e| TxIngressError::Other(format!("Failed to calculate term fee: {}", e)))
    }
}

pub type AtomicMempoolState = Arc<RwLock<MempoolState>>;
#[derive(Debug)]
pub struct MempoolState {
    /// valid submit txs
    pub valid_submit_ledger_tx: BTreeMap<H256, DataTransactionHeader>,
    pub valid_commitment_tx: BTreeMap<Address, Vec<CommitmentTransaction>>,
    /// The miner's signer instance, used to sign ingress proofs
    pub recent_invalid_tx: LruCache<H256, ()>,
    /// Tracks recent valid txids from either data or commitment
    pub recent_valid_tx: LruCache<H256, ()>,
    /// Tracks recently processed chunk hashes to prevent re-gossip
    pub recent_valid_chunks: LruCache<ChunkPathHash, ()>,
    /// LRU caches for out of order gossip data
    pub pending_chunks: LruCache<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    pub pending_pledges: LruCache<Address, LruCache<IrysTransactionId, CommitmentTransaction>>,
    /// pre-validated blocks that have passed pre-validation in discovery service
    pub prevalidated_blocks: HashMap<H256, IrysBlockHeader>,
    pub prevalidated_blocks_poa: HashMap<H256, Base64>,
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(config: &MempoolConfig) -> MempoolState {
    let max_pending_chunk_items = config.max_pending_chunk_items;
    let max_pending_pledge_items = config.max_pending_pledge_items;
    MempoolState {
        prevalidated_blocks: HashMap::new(),
        prevalidated_blocks_poa: HashMap::new(),
        valid_submit_ledger_tx: BTreeMap::new(),
        valid_commitment_tx: BTreeMap::new(),
        recent_invalid_tx: LruCache::new(NonZeroUsize::new(config.max_invalid_items).unwrap()),
        recent_valid_tx: LruCache::new(NonZeroUsize::new(config.max_valid_items).unwrap()),
        recent_valid_chunks: LruCache::new(NonZeroUsize::new(config.max_valid_chunks).unwrap()),
        pending_chunks: LruCache::new(NonZeroUsize::new(max_pending_chunk_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
    }
}

/// Reasons why reading a transaction might fail
#[derive(Debug, Clone)]
pub enum TxReadError {
    /// Some database error occurred when reading
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// The commitment transaction is not found in the mempool
    CommitmentTxNotInMempool,
    /// The transaction is not found in the mempool
    DataTxNotInMempool,
    /// Catch-all variant for other errors.
    Other(String),
}

impl TxReadError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    #[error("Transaction signature is invalid")]
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    #[error("Account has insufficient funds for this transaction")]
    Unfunded,
    /// This transaction id is already in the cache
    #[error("Transaction already exists in cache")]
    Skipped,
    /// Invalid anchor value (unknown or too old)
    #[error("Anchor is either unknown or has expired")]
    InvalidAnchor,
    /// Invalid ledger type specified in transaction
    #[error("Invalid or unsupported ledger ID: {0}")]
    InvalidLedger(u32),
    /// Some database error occurred
    #[error("Database operation failed")]
    DatabaseError,
    /// The service is uninitialized
    #[error("Mempool service is not initialized")]
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    #[error("Transaction ingress error: {0}")]
    Other(String),
    /// Commitment transaction validation error
    #[error("Commitment validation failed: {0}")]
    CommitmentValidationError(#[from] CommitmentValidationError),
    /// Failed to fetch account balance from RPC
    #[error("Failed to fetch balance for address {address}: {reason}")]
    BalanceFetchError { address: String, reason: String },
}

impl TxIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct MempoolTxs {
    pub commitment_tx: Vec<CommitmentTransaction>,
    pub submit_tx: Vec<DataTransactionHeader>,
    pub publish_tx: PublishLedgerWithTxs,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngressProofError {
    /// The proofs signature is invalid
    #[error("Ingress proof signature is invalid")]
    InvalidSignature,
    /// There was a database error storing the proof
    #[error("Database error")]
    DatabaseError,
    /// The proof does not come from a staked address
    #[error("Unstaked address")]
    UnstakedAddress,
    /// Catch-all variant for other errors.
    #[error("Ingress proof error: {0}")]
    Other(String),
}

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<MempoolServiceMessage>, // mempool message receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,        // reorg broadcast receiver
    block_migrated_rx: broadcast::Receiver<BlockMigratedEvent>, // block broadcast migrated receiver
    inner: Inner,
}

impl Default for MempoolService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation of the `MempoolService`");
    }
}

impl MempoolService {
    /// Spawn a new Mempool service
    pub fn spawn_service(
        irys_db: DatabaseProvider,
        reth_node_adapter: IrysRethNodeAdapter,
        storage_modules_guard: StorageModulesReadGuard,
        block_tree_read_guard: &BlockTreeReadGuard,
        rx: UnboundedReceiver<MempoolServiceMessage>,
        config: &Config,
        service_senders: &ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
    ) -> eyre::Result<TokioServiceHandle> {
        info!("Spawning mempool service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.mempool;
        let mempool_state = create_state(mempool_config);
        let storage_modules_guard = storage_modules_guard;
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let block_migrated_rx = service_senders.subscribe_block_migrated();

        let handle = runtime_handle.spawn(
            async move {
                let mempool_state = Arc::new(RwLock::new(mempool_state));
                let pledge_provider = MempoolPledgeProvider::new(
                    mempool_state.clone(),
                    block_tree_read_guard.clone(),
                );

                let mempool_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    reorg_rx,
                    block_migrated_rx,
                    inner: Inner {
                        block_tree_read_guard,
                        config,
                        exec: TaskExecutor::current(),
                        irys_db,
                        mempool_state,
                        reth_node_adapter,
                        service_senders,
                        storage_modules_guard,
                        pledge_provider,
                    },
                };
                mempool_service
                    .start()
                    .await
                    .expect("Mempool service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        Ok(TokioServiceHandle {
            name: "mempool_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        })
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting Mempool service");

        self.inner.restore_mempool_from_disk().await;

        let mut shutdown_future = pin!(self.shutdown);
        loop {
            tokio::select! {
                // Handle regular mempool messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            tracing::warn!("receiver channel closed");
                            break;
                        }
                    }
                }

                // Handle reorg events
                reorg_result = self.reorg_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(reorg_result, "Reorg") {
                        self.inner.handle_reorg(event).await?;
                    }
                }

                // Handle block migrated events
                 migrated_result = self.block_migrated_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(migrated_result, "BlockMigrated") {
                        self.inner.handle_block_migrated(event).await?;
                    }
                }


                // Handle shutdown signal
                _ = &mut shutdown_future => {
                    info!("Shutdown signal received for mempool service");
                    break;
                }
            }
        }

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        self.inner.persist_mempool_to_disk().await?;

        tracing::info!("shutting down Mempool service");
        Ok(())
    }
}

pub fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
    channel_name: &str,
) -> Option<T> {
    match result {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            tracing::debug!("{} channel closed", channel_name);
            None
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!("{} lagged by {} events", channel_name, n);
            if n > 5 {
                tracing::error!("{} significantly lagged", channel_name);
            }
            None
        }
    }
}
