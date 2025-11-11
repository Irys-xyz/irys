pub mod chunks;
pub mod commitment_txs;
pub mod data_txs;
pub mod facade;
pub mod ingress_proofs;
pub mod lifecycle;
pub mod pledge_provider;
pub mod types;

pub use chunks::*;
pub use facade::*;
pub use types::*;

use crate::block_discovery::get_data_tx_in_parallel_inner;
use crate::block_tree_service::{BlockMigratedEvent, ReorgEvent};
use crate::block_validation::{calculate_perm_storage_total_fee, get_assigned_ingress_proofs};
use crate::pledge_provider::MempoolPledgeProvider;
use crate::services::ServiceSenders;
use crate::shadow_tx_generator::PublishLedgerWithTxs;
use eyre::{eyre, OptionExt as _};
use futures::future::BoxFuture;
use futures::FutureExt as _;
use irys_database::tables::IngressProofs;
use irys_database::{
    cached_data_root_by_data_root, ingress_proofs_by_data_root, tx_header_by_txid, SystemLedger,
};
use irys_domain::{
    get_atomic_file, BlockTreeEntry, BlockTreeReadGuard, CommitmentSnapshotStatus,
    StorageModulesReadGuard,
};
use irys_reth_node_bridge::{ext::IrysRethRpcTestContextExt as _, IrysRethNodeAdapter};
use irys_storage::RecoveredMempoolState;
use irys_types::ingress::IngressProof;
use irys_types::transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges};
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
    Address, ChunkPathHash, CommitmentTransaction, CommitmentValidationError, DataRoot,
    DataTransactionHeader, MempoolConfig, TxChunkOffset, UnpackedChunk,
};
use irys_types::{BlockHash, CommitmentType};
use irys_types::{DataLedger, IngressProofsList, TokioServiceHandle, TxKnownStatus};
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
use tokio::sync::{broadcast, mpsc::UnboundedReceiver, oneshot, RwLock};
use tracing::{debug, error, info, instrument, trace, warn, Instrument as _};

/// Public helper to validate that a commitment transaction is sufficiently funded.
/// Checks the current balance of the signer via the provided reth adapter and ensures it
/// covers the total cost (value + fee) of the transaction.
#[inline]
pub fn validate_funding(
    reth_adapter: &IrysRethNodeAdapter,
    commitment_tx: &irys_types::CommitmentTransaction,
    parent_evm_block_id: Option<BlockId>,
) -> Result<(), TxIngressError> {
    // Fetch the current balance of the signer
    let balance: irys_types::U256 = reth_adapter
        .rpc
        .get_balance_irys_canonical_and_pending(commitment_tx.signer, parent_evm_block_id)
        .map_err(|e| {
            tracing::error!(
                tx.id = %commitment_tx.id,
                tx.signer = %commitment_tx.signer,
                tx.error = %e,
                "Failed to fetch balance for commitment tx"
            );
            TxIngressError::BalanceFetchError {
                address: commitment_tx.signer.to_string(),
                reason: e.to_string(),
            }
        })?;

    let required = commitment_tx.total_cost();

    if balance < required {
        tracing::warn!(
            tx.id = %commitment_tx.id,
            account.balance = %balance,
            tx.required_balance = %required,
            tx.signer = %commitment_tx.signer,
            "Insufficient balance for commitment tx"
        );
        return Err(TxIngressError::Unfunded);
    }

    tracing::debug!(
        tx.id = %commitment_tx.id,
        account.balance = %balance,
        tx.required_balance = %required,
        "Funding validated for commitment tx"
    );

    Ok(())
}

/// Public helper to validate a commitment transaction's basic invariants used during
/// block discovery and mempool selection:
/// - fee must meet minimum requirements
/// - account must have enough funds at the specified EVM block state (Pass None for new incoming
///   transactions - this will validate against the current canonical tip)
/// - value must match the commitment type rules
#[inline]
pub fn validate_commitment_transaction(
    reth_adapter: &IrysRethNodeAdapter,
    consensus: &irys_types::ConsensusConfig,
    commitment_tx: &irys_types::CommitmentTransaction,
    parent_evm_block_id: Option<BlockId>,
) -> Result<(), TxIngressError> {
    debug!(
        tx.id = ?commitment_tx.id,
        tx.signer = ?commitment_tx.signer,
        "Validating commitment transaction"
    );
    // Fee
    commitment_tx.validate_fee(consensus).map_err(|e| {
        warn!(
            tx.id = ?commitment_tx.id,
            tx.signer = ?commitment_tx.signer,
            tx.error = ?e,
            "Commitment tx fee validation failed"
        );
        TxIngressError::from(e)
    })?;

    // Funding
    validate_funding(reth_adapter, commitment_tx, parent_evm_block_id).map_err(|e| {
        warn!(
            tx.id = ?commitment_tx.id,
            tx.signer = ?commitment_tx.signer,
            tx.error = ?e,
            "Commitment tx funding validation failed"
        );
        e
    })?;

    // Value
    commitment_tx.validate_value(consensus).map_err(|e| {
        warn!(
            tx.id = ?commitment_tx.id,
            tx.signer = ?commitment_tx.signer,
            tx.error = ?e,
            "Commitment tx value validation failed"
        );
        TxIngressError::from(e)
    })?;
    Ok(())
}

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
    pub stake_and_pledge_whitelist: HashSet<Address>,
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
    IngestChunkFireAndForget(UnpackedChunk),
    IngestIngressProof(IngressProof, oneshot::Sender<Result<(), IngressProofError>>),

    /// Confirm commitment tx exists in mempool
    CommitmentTxExists(H256, oneshot::Sender<Result<TxKnownStatus, TxReadError>>),
    /// Ingress CommitmentTransaction into the mempool (from API)
    ///
    /// This function performs a series of checks and validations:
    /// - Skips the transaction if it is already known to be invalid or previously processed
    /// - Validates the transaction's anchor and signature
    /// - Inserts the valid transaction into the mempool and database
    /// - Processes any pending pledge transactions that depended on this commitment
    /// - Gossips the transaction to peers if accepted
    /// - Caches the transaction for unstaked signers to be reprocessed later
    IngestCommitmentTxFromApi(
        CommitmentTransaction,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Ingress CommitmentTransaction into the mempool (from Gossip)
    IngestCommitmentTxFromGossip(
        CommitmentTransaction,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Confirm data tx exists in mempool or database
    DataTxExists(H256, oneshot::Sender<Result<TxKnownStatus, TxReadError>>),
    /// validate and process an incoming DataTransactionHeader (from API)
    IngestDataTxFromApi(
        DataTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// validate and process an incoming DataTransactionHeader (from Gossip)
    IngestDataTxFromGossip(
        DataTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Return filtered list of candidate txns
    /// Filtering based on funding status etc based on the provided EVM block ID
    /// If `None` is provided, the latest canonical block is used
    GetBestMempoolTxs(Option<BlockId>, oneshot::Sender<eyre::Result<MempoolTxs>>),
    // todo update the test utils to use the read guard instead. Then this can be deleted
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
    GetState(oneshot::Sender<AtomicMempoolState>),
    /// Remove the set of txids from any blocklists (recent_invalid_txs)
    RemoveFromBlacklist(Vec<H256>, oneshot::Sender<()>),
    UpdateStakeAndPledgeWhitelist(HashSet<Address>, oneshot::Sender<()>),
    GetStakeAndPledgeWhitelist(oneshot::Sender<HashSet<Address>>),
    /// Get overall mempool status and metrics
    GetMempoolStatus(oneshot::Sender<Result<MempoolStatus, TxReadError>>),
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
                    if let Err(e) = self.handle_block_confirmed_message(block).await {
                        tracing::error!("Failed to handle block confirmed message: {:#}", e);
                    }
                }
                MempoolServiceMessage::IngestCommitmentTxFromApi(commitment_tx, response) => {
                    let response_message = self
                        .handle_ingress_commitment_tx_message_api(commitment_tx)
                        .await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::IngestCommitmentTxFromGossip(commitment_tx, response) => {
                    let response_message = self
                        .handle_ingress_commitment_tx_message_gossip(commitment_tx)
                        .await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::IngestChunk(chunk, response) => {
                    let response_value: Result<(), ChunkIngressError> =
                        self.handle_chunk_ingress_message(chunk).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!(
                            "handle_chunk_ingress_message response.send() error: {:?}",
                            e
                        );
                    };
                }
                MempoolServiceMessage::IngestChunkFireAndForget(chunk) => {
                    let result = self.handle_chunk_ingress_message(chunk).await;
                    if let Err(e) = result {
                        tracing::error!("handle_chunk_ingress_message error: {:?}", e);
                    }
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
                MempoolServiceMessage::IngestDataTxFromApi(tx, response) => {
                    let response_value = self.handle_data_tx_ingress_message_api(tx).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::IngestDataTxFromGossip(tx, response) => {
                    let response_value = self.handle_data_tx_ingress_message_gossip(tx).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }

                MempoolServiceMessage::GetState(response) => {
                    if let Err(e) = response
                        .send(Arc::clone(&self.mempool_state))
                        .inspect_err(|e| tracing::error!("response.send() error: {:?}", e))
                    {
                        tracing::error!("response.send() error: {:?}", e);
                    }
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
                MempoolServiceMessage::GetMempoolStatus(response) => {
                    let response_value = self.handle_get_mempool_status().await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::UpdateStakeAndPledgeWhitelist(new_entries, response) => {
                    self.stake_and_pledge_whitelist.extend(new_entries);
                    if let Err(e) = response.send(()) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetStakeAndPledgeWhitelist(tx) => {
                    let whitelist = self.stake_and_pledge_whitelist.clone();
                    if let Err(e) = tx.send(whitelist) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
            }
            Ok(())
        })
    }

    async fn handle_get_mempool_status(&self) -> Result<MempoolStatus, TxReadError> {
        let state = self.mempool_state.read().await;

        // Calculate total data size
        let data_tx_total_size: u64 = state
            .valid_submit_ledger_tx
            .values()
            .map(|tx| tx.data_size)
            .sum();

        let mempool_config = &self.config.node_config.consensus_config().mempool;

        // Calculate capacity utilization
        let data_tx_capacity_pct = if state.max_submit_txs > 0 {
            (state.valid_submit_ledger_tx.len() as f64 / state.max_submit_txs as f64) * 100.0
        } else {
            0.0
        };

        let commitment_address_capacity_pct = if state.max_commitment_addresses > 0 {
            (state.valid_commitment_tx.len() as f64 / state.max_commitment_addresses as f64) * 100.0
        } else {
            0.0
        };

        // Log capacity warnings
        if data_tx_capacity_pct > 90.0 {
            warn!(
                mempool.data_tx_capacity = data_tx_capacity_pct,
                mempool.data_tx_count = state.valid_submit_ledger_tx.len(),
                mempool.data_tx_max = state.max_submit_txs,
                "Data tx mempool approaching capacity"
            );
        }

        if commitment_address_capacity_pct > 90.0 {
            warn!(
                mempool.commitment_address_capacity = commitment_address_capacity_pct,
                mempool.commitment_address_count = state.valid_commitment_tx.len(),
                mempool.commitment_address_max = state.max_commitment_addresses,
                "Commitment address mempool approaching capacity"
            );
        }

        debug!(
            mempool.data_tx_capacity = data_tx_capacity_pct,
            mempool.commitment_address_capacity = commitment_address_capacity_pct,
            "Mempool capacity utilization"
        );

        Ok(MempoolStatus {
            data_tx_count: state.valid_submit_ledger_tx.len(),
            commitment_tx_count: state.valid_commitment_tx.values().map(Vec::len).sum(),
            pending_chunks_count: state.pending_chunks.len(),
            pending_pledges_count: state.pending_pledges.len(),
            recent_valid_tx_count: state.recent_valid_tx.len(),
            recent_invalid_tx_count: state.recent_invalid_tx.len(),
            data_tx_total_size,
            config: mempool_config.clone(),
            data_tx_capacity_pct,
            commitment_address_capacity_pct,
        })
    }

    async fn remove_from_blacklists(&mut self, tx_ids: Vec<H256>) {
        let mut state = self.mempool_state.write().await;
        for tx_id in tx_ids {
            state.recent_invalid_tx.pop(&tx_id);
        }
    }

    #[instrument(skip_all)]
    pub async fn validate_tx_anchor_for_inclusion(
        &self,
        min_anchor_height: u64,
        max_anchor_height: u64,
        tx: &impl IrysTransactionCommon,
    ) -> eyre::Result<bool> {
        let tx_id = tx.id();
        let anchor = tx.anchor();
        // ingress proof anchors must be canonical for inclusion
        let anchor_height = match self
            .get_anchor_height(anchor, true)
            .map_err(|_e| TxIngressError::DatabaseError)?
        {
            Some(height) => height,
            None => {
                Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
                return Err(TxIngressError::InvalidAnchor.into());
            }
        };

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

    #[instrument(skip_all)]
    pub fn validate_ingress_proof_anchor_for_inclusion(
        &self,
        min_anchor_height: u64,
        ingress_proof: &IngressProof,
    ) -> eyre::Result<bool> {
        let anchor = ingress_proof.anchor;
        let anchor_height = match self
            .get_anchor_height(anchor, true)
            .map_err(|_e| TxIngressError::DatabaseError)?
        {
            Some(height) => height,
            None => {
                // Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
                return Ok(false);
            }
        };

        // these have to be inclusive so we handle txs near height 0 correctly
        let new_enough = anchor_height >= min_anchor_height;

        // note: we don't need old_enough as we're part of the block header
        // so there's no need to go through the mempool
        // let old_enough: bool = anchor_height <= max_anchor_height;
        if new_enough {
            Ok(true)
        } else {
            // TODO: recover the signer's address here? (or compute an ID)
            warn!("ingress proof data_root {} signature {:?} anchor {anchor} has height {anchor_height}, which is too old compared to min height {min_anchor_height}", &ingress_proof.data_root, &ingress_proof.signature);
            Ok(false)
        }
    }

    #[instrument(skip(self), fields(parent_block.id = ?parent_evm_block_id), err)]
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
            (self.config.consensus.mempool.tx_anchor_expiry_depth as u64)
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
                    tx.signer = ?signer,
                    account.balance = ?balance,
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
            chain.head_height = last_block.height,
            chain.head_hash = ?last_block.block_hash,
            chain.canonical_length = canonical.len(),
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
            .flat_map(|txs| txs.iter().cloned())
            .collect::<Vec<_>>();

        // Sort all commitments according to our priority rules
        sorted_commitments.sort();

        // Process sorted commitments
        // create a throw away commitment snapshot so we can simulate behaviour before including a commitment tx in returned txs
        let mut simulation_commitment_snapshot = commitment_snapshot.as_ref().clone();
        for tx in &sorted_commitments {
            if confirmed_commitments.contains(&tx.id) {
                debug!(
                    tx.id = ?tx.id,
                    tx.commitment_type = ?tx.commitment_type,
                    tx.signer = ?tx.signer,
                    "Skipping already confirmed commitment transaction"
                );
                continue;
            }

            // Full validation (fee, funding at parent block, value) before simulation
            if let Err(error) = validate_commitment_transaction(
                &self.reth_node_adapter,
                &self.config.consensus,
                tx,
                parent_evm_block_id,
            ) {
                tracing::warn!(tx.error = ?error, "rejecting commitment tx");
                continue;
            }

            if !self
                .validate_tx_anchor_for_inclusion(min_anchor_height, max_anchor_height, tx)
                .await?
            {
                continue;
            }

            // signer stake status check
            if matches!(tx.commitment_type, CommitmentType::Stake) {
                let is_staked = epoch_snapshot.is_staked(tx.signer);
                debug!(
                    tx.id = ?tx.id,
                    tx.signer = ?tx.signer,
                    tx.is_staked = is_staked,
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
                        tx.commitment_type = ?tx.commitment_type,
                        tx.id = ?tx.id,
                        tx.simulation_status = ?simulation,
                        "Commitment tx rejected by simulation"
                    );
                    continue;
                }
            }

            debug!(
                tx.id = ?tx.id,
                tx.commitment_type = ?tx.commitment_type,
                tx.signer = ?tx.signer,
                tx.fee = ?tx.total_cost(),
                tx.selected_count = commitment_tx.len() + 1,
                tx.max_commitments = max_commitments,
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
                commitment_selection.selected_commitments = commitment_tx.len(),
                commitment_selection.stake_txs = commitment_summary.0,
                commitment_selection.pledge_txs = commitment_summary.1,
                commitment_selection.max_allowed = max_commitments,
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
                    tx.id = ?tx.id,
                    tx.ledger_id = tx.ledger_id,
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
                            tx.id = ?tx.id,
                            tx.actual_term_fee = ?tx.term_fee,
                            tx.expected_term_fee = ?expected_term_fee,
                            "Skipping Publish tx: insufficient term_fee"
                        );
                        continue;
                    }

                    // Validate perm fee must be present for Publish ledger
                    let Some(perm_fee) = tx.perm_fee else {
                        // Missing perm_fee for Publish ledger transaction is invalid
                        warn!(
                            tx.id = ?tx.id,
                            tx.signer = ?tx.signer,
                            "Invalid Publish tx: missing perm_fee"
                        );
                        // todo: add to list of invalid txs because all publish txs must have perm fee present
                        continue;
                    };
                    if perm_fee < expected_perm_fee.amount {
                        trace!(
                            tx.id = ?tx.id,
                            tx.actual_perm_fee = ?perm_fee,
                            tx.expected_perm_fee = ?expected_perm_fee.amount,
                            "Skipping Publish tx: insufficient perm_fee"
                        );
                        continue;
                    }

                    // Mirror API-only structural checks to filter gossip txs that would fail API validation
                    if TermFeeCharges::new(tx.term_fee, &self.config.node_config.consensus_config())
                        .is_err()
                    {
                        trace!(
                            tx.id = ?tx.id,
                            tx.term_fee = ?tx.term_fee,
                            "Skipping Publish tx: invalid term fee structure"
                        );
                        continue;
                    }

                    if PublishFeeCharges::new(
                        perm_fee,
                        tx.term_fee,
                        &self.config.node_config.consensus_config(),
                    )
                    .is_err()
                    {
                        trace!(
                            tx.id = ?tx.id,
                            tx.perm_fee = ?perm_fee,
                            tx.term_fee = ?tx.term_fee,
                            "Skipping Publish tx: invalid perm fee structure"
                        );
                        continue;
                    }
                }
                irys_types::DataLedger::Submit => {
                    // todo: add to list of invalid txs because we don't support Submit txs
                }
            }

            if !self
                .validate_tx_anchor_for_inclusion(min_anchor_height, max_anchor_height, &tx)
                .await?
            {
                continue;
            }

            trace!(
                tx.id = ?tx.id,
                tx.signer = ?tx.signer(),
                tx.fee = ?tx.total_cost(),
                "Checking funding for data transaction"
            );
            if check_funding(&tx) {
                trace!(
                    tx.id = ?tx.id,
                    tx.signer = ?tx.signer(),
                    tx.fee = ?tx.total_cost(),
                    tx.selected_count = submit_tx.len() + 1,
                    tx.max_data_txs = max_data_txs,
                    "Data transaction passed funding check"
                );
                submit_tx.push(tx);
                if submit_tx.len() >= max_data_txs {
                    break;
                }
            } else {
                trace!(
                    tx.id = ?tx.id,
                    tx.signer = ?tx.signer(),
                    tx.fee = ?tx.total_cost(),
                    tx.validation_failed_reason = "insufficient_funds",
                    "Data transaction failed funding check"
                );
            }
        }

        // note: publish txs are sorted internally by the get_publish_txs_and_proofs fn
        let publish_txs_and_proofs = self
            .get_publish_txs_and_proofs(&canonical, &submit_tx, current_height)
            .await?;

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
            mempool_selected.commitment_txs = commitment_tx.len(),
            mempool_selected.data_txs = submit_tx.len(),
            mempool_selected.publish_txs = publish_txs_and_proofs.txs.len(),
            mempool_selected.total_fee_collected = ?total_fee_collected,
            mempool_selected.unfunded_addresses = unfunded_address.len(),
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
                    mempool_selected.rejection_rate = rejection_rate,
                    mempool_selected.total_available = total_available,
                    mempool_selected.total_selected = total_selected,
                    mempool_selected.commitments_available = total_commitments_available,
                    mempool_selected.commitments_selected = commitment_tx.len(),
                    mempool_selected.data_available = total_data_available,
                    mempool_selected.data_selected = submit_tx.len(),
                    mempool_selected.unfunded_addresses = unfunded_address.len(),
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

    pub async fn get_publish_txs_and_proofs(
        &self,
        canonical: &[BlockTreeEntry],
        submit_tx: &[DataTransactionHeader],
        current_height: u64,
    ) -> Result<PublishLedgerWithTxs, eyre::Error> {
        let mut publish_txs: Vec<DataTransactionHeader> = Vec::new();
        let mut publish_proofs: Vec<IngressProof> = Vec::new();

        // only max anchor age is constrained for ingress proofs
        let min_ingress_proof_anchor_height = current_height.saturating_sub(
            self.config
                .consensus
                .mempool
                .ingress_proof_anchor_expiry_depth as u64,
        );

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

            // Loop through all the data_roots with ingress proofs and find corresponding transaction ids
            for data_root in ingress_proofs.keys() {
                let cached_data_root = cached_data_root_by_data_root(&read_tx, *data_root).unwrap();
                if let Some(cached_data_root) = cached_data_root {
                    let txids = cached_data_root.txid_set;
                    debug!(tx.ids = ?txids, "Publish candidates");
                    publish_txids.extend(txids)
                }
            }

            // Loop through all the pending tx to see which haven't been promoted
            let txs = self.handle_get_data_tx_message(publish_txids.clone()).await;

            // Note: get_data_tx_in_parallel_inner() read from both the mempool and
            //       db as publishing can happen to a tx that is no longer in the mempool
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

            // Sort the resulting publish_txs & proofs
            tx_headers.sort_by(|a, b| a.id.cmp(&b.id));

            // reduce down the canonical chain to the txs in the submit ledger
            let submit_txs_from_canonical = canonical.iter().fold(HashSet::new(), |mut acc, v| {
                acc.extend(v.data_ledgers[&DataLedger::Submit].0.clone());
                acc
            });
            let ro_tx = self.irys_db.tx()?;

            for tx_header in &tx_headers {
                debug!(
                    "Processing publish candidate tx {} {:#?}",
                    &tx_header.id, &tx_header
                );
                let is_promoted = tx_header.promoted_height.is_some();

                if is_promoted {
                    // If it's promoted skip it
                    warn!(
                        "Publish candidate {} is already promoted? {}",
                        &tx_header.id, &is_promoted
                    );
                    continue;
                }
                // check for previous submit inclusion
                // we do this by checking if the tx is in the block tree or database.
                // if it is, we know it could've only gotten there by being included in the submit ledger.
                // if it's not, we also check if the submit ledger for this block contains the tx (single-block promotion). if it does, we also promote it.
                if !submit_txs_from_canonical.contains(&tx_header.id) {
                    // check for single-block promotion
                    if !submit_tx.iter().any(|tx| tx.id == tx_header.id) {
                        // check database
                        if tx_header_by_txid(&ro_tx, &tx_header.id)?.is_none() {
                            // no previous inclusion
                            warn!(
                                "Unable to find previous submit inclusion for publish candidate {}",
                                &tx_header.id
                            );
                            continue;
                        }
                    }
                }

                // If it's not promoted, validate the proofs

                // Get all the proofs for this tx
                let all_proofs = ingress_proofs_by_data_root(&read_tx, tx_header.data_root)?;

                // Check for minimum number of ingress proofs
                let total_miners = self
                    .block_tree_read_guard
                    .read()
                    .canonical_epoch_snapshot()
                    .commitment_state
                    .stake_commitments
                    .len();

                // Take the smallest value, the configured total proofs count or the number
                // of staked miners that can produce a valid proof.
                let proofs_per_tx = std::cmp::min(
                    self.config.consensus.number_of_ingress_proofs_total as usize,
                    total_miners,
                );

                if all_proofs.len() < proofs_per_tx {
                    info!(
                        "Not promoting tx {} - insufficient proofs (got {} wanted {})",
                        &tx_header.id,
                        &all_proofs.len(),
                        proofs_per_tx
                    );
                    continue;
                }

                let mut all_tx_proofs: Vec<IngressProof> = Vec::with_capacity(all_proofs.len());

                //filter all these ingress proofs by their anchor validity
                for (_hash, proof) in all_proofs {
                    let proof = proof.0.proof;
                    // validate the anchor is still valid
                    let anchor_is_valid = self.validate_ingress_proof_anchor_for_inclusion(
                        min_ingress_proof_anchor_height,
                        &proof,
                    )?;
                    if anchor_is_valid {
                        all_tx_proofs.push(proof)
                    }
                    // note: data root lifecycle work includes code to handle ingress proofs we find as invalid
                }

                // Get assigned and unassigned proofs using the existing utility function
                let (assigned_proofs, assigned_miners) = match get_assigned_ingress_proofs(
                    &all_tx_proofs,
                    tx_header,
                    |hash| self.handle_get_block_header_message(hash, false), // Closure captures self
                    &self.block_tree_read_guard,
                    &self.irys_db,
                    &self.config,
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        warn!(
                            "Failed to get assigned proofs for tx {}: {}",
                            &tx_header.id, e
                        );
                        continue;
                    }
                };

                // Calculate expected assigned proofs, clamping to available miners
                let mut expected_assigned_proofs =
                    self.config
                        .consensus
                        .number_of_ingress_proofs_from_assignees as usize;

                if assigned_miners < expected_assigned_proofs {
                    warn!(
                        "Clamping expected_assigned_proofs from {} to {} for tx {}",
                        expected_assigned_proofs, assigned_miners, &tx_header.id
                    );
                    expected_assigned_proofs = assigned_miners;
                }

                // Check if we have enough assigned proofs
                if assigned_proofs.len() < expected_assigned_proofs {
                    info!(
                        "Not promoting tx {} - insufficient assigned proofs (got {} wanted {})",
                        &tx_header.id,
                        assigned_proofs.len(),
                        expected_assigned_proofs
                    );
                    continue;
                }

                // Separate assigned and unassigned proofs
                let assigned_proof_set: HashSet<_> = assigned_proofs
                    .iter()
                    .map(|p| &p.proof.0) // Use signature as unique identifier
                    .collect();

                let unassigned_proofs: Vec<IngressProof> = all_tx_proofs
                    .iter()
                    .filter(|p| !assigned_proof_set.contains(&p.proof.0))
                    .cloned()
                    .collect();

                // Build the final proof list
                let mut final_proofs = Vec::new();

                // First, add assigned proofs up to the total network limit
                // Use all available assigned proofs, but don't exceed the network total
                let total_network_limit =
                    self.config.consensus.number_of_ingress_proofs_total as usize;
                let assigned_to_use = std::cmp::min(assigned_proofs.len(), total_network_limit);
                final_proofs.extend_from_slice(&assigned_proofs[..assigned_to_use]);

                // Then fill remaining slots with unassigned proofs if needed
                let remaining_slots = total_network_limit - final_proofs.len();
                if remaining_slots > 0 {
                    let unassigned_to_use = std::cmp::min(unassigned_proofs.len(), remaining_slots);
                    final_proofs.extend_from_slice(&unassigned_proofs[..unassigned_to_use]);
                }

                // Final check - do we have enough total proofs?
                if final_proofs.len()
                    < self.config.consensus.number_of_ingress_proofs_total as usize
                {
                    info!(
                            "Not promoting tx {} - insufficient total proofs after assignment filtering (got {} wanted {})",
                            &tx_header.id,
                            final_proofs.len(),
                            self.config.consensus.number_of_ingress_proofs_total
                        );
                    continue;
                }

                // Success - add this transaction and its proofs
                publish_txs.push(tx_header.clone());
                publish_proofs.extend(final_proofs.clone()); // Clone to avoid moving final_proofs

                info!(
                    "Promoting tx {} with {} assigned proofs and {} total proofs",
                    &tx_header.id,
                    assigned_to_use, // Show actual assigned proofs used (capped by network limit)
                    final_proofs.len()
                );
            }
        }

        let txs = &publish_txs.iter().map(|h| h.id).collect::<Vec<_>>();
        debug!(tx.ids = ?txs, "Publish transactions");

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
    /// TODO: we can remove this function and replace call sites with direct use of a block tree guard
    #[expect(clippy::unused_async)]
    pub async fn handle_get_block_header_message(
        &self,
        block_hash: H256,
        include_chunk: bool,
    ) -> Option<IrysBlockHeader> {
        let guard = self.block_tree_read_guard.read();
        let mut block = guard.get_block(&block_hash).cloned();

        if !include_chunk {
            if let Some(ref mut b) = block {
                b.poa.chunk = None
            }
        }
        block
    }

    // Resolves an anchor (block hash) to it's height
    // if it couldn't find the anchor, returns None
    // set canonical to true to enforce that the anchor must be part of the current canonical chain
    pub fn get_anchor_height(&self, anchor: H256, canonical: bool) -> eyre::Result<Option<u64>> {
        // check the mempool, then block tree, then DB

        if let Some(height) = {
            // in a block so rust doesn't complain about it being held across an await point
            // I suspect if let Some desugars to something that lint doesn't like
            let guard = self.block_tree_read_guard.read();
            if canonical {
                guard
                    .get_canonical_chain()
                    .0
                    .iter()
                    .find(|b| b.block_hash == anchor)
                    .map(|b| b.height)
            } else {
                guard.get_block(&anchor).map(|h| h.height)
            }
        } {
            Ok(Some(height))
        } else if let Some(hdr) = {
            let read_tx = self.read_tx()?;
            irys_database::block_header_by_hash(&read_tx, &anchor, false)?
        } {
            Ok(Some(hdr.height))
        } else {
            // Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
            // return Err(TxIngressError::InvalidAnchor);
            Ok(None)
        }
    }

    // Helper to validate anchor
    // this takes in an IrysTransaction and validates the anchor
    // if the anchor is valid, returns anchor block height
    #[instrument(skip_all, fields(tx.id = %tx.id(), anchor = %tx.anchor()))]
    pub async fn validate_tx_anchor(
        &self,
        tx: &impl IrysTransactionCommon,
    ) -> Result<u64, TxIngressError> {
        let tx_id = tx.id();
        let anchor = tx.anchor();

        let latest_height = self.get_latest_block_height()?;

        // let anchor_height = self.get_anchor_height(tx_id, anchor).await?;

        let anchor_height = match self
            .get_anchor_height(anchor, false /* does not need to be canonical */)
            .map_err(|_e| TxIngressError::DatabaseError)?
        {
            Some(height) => height,
            None => {
                Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
                return Err(TxIngressError::InvalidAnchor);
            }
        };

        // is this anchor too old?

        let min_anchor_height = latest_height
            .saturating_sub(self.config.consensus.mempool.tx_anchor_expiry_depth as u64);

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
            if let Err(e) = self
                .handle_ingress_commitment_tx_message_gossip(commitment_tx)
                .await
                .inspect_err(|_| {
                    tracing::warn!("Commitment tx ingress error during mempool restore from disk")
                })
            {
                tracing::warn!(
                    "Commitment tx ingress error during mempool restore from disk: {:?}",
                    e
                );
            }
        }

        for (_txid, storage_tx) in recovered.storage_txs {
            if let Err(e) = self
                .handle_data_tx_ingress_message_gossip(storage_tx)
                .await
                .inspect_err(|_| {
                    tracing::warn!("Storage tx ingress error during mempool restore from disk")
                })
            {
                tracing::warn!(
                    "Storage tx ingress error during mempool restore from disk: {:?}",
                    e
                );
            }
        }

        self.wipe_blacklists().await;
    }

    // wipes all the "blacklists", primarily used after trying to restore the mempool from disk so that validation errors then (i.e if we have a saved tx that uses an anchor from some blocks that we forgot we when restarted) don't affect block validation
    // right now this only wipes `recent_invalid_tx`
    pub async fn wipe_blacklists(&mut self) {
        let mut write = self.mempool_state.write().await;
        write.recent_invalid_tx.clear();
        write.recent_invalid_payload_fingerprints.clear();
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
    #[instrument(skip_all, fields(tx.id = %tx.id()))]
    pub async fn validate_signature<
        T: irys_types::versioning::Signable
            + IrysTransactionCommon
            + std::fmt::Debug
            + serde::Serialize,
    >(
        &mut self,
        tx: &T,
    ) -> Result<(), TxIngressError> {
        if tx.is_signature_valid() {
            info!(
                "Tx {} signature is valid for signer {}",
                &tx.id(),
                &tx.signer()
            );
            Ok(())
        } else {
            // Record invalid payload fingerprint derived from both the signature and the
            // signing preimage (prehash). This avoids poisoning legitimate transactions that
            // share the same signature bytes but differ in signed content.
            let fingerprint = tx.fingerprint();
            self.mempool_state
                .write()
                .await
                .recent_invalid_payload_fingerprints
                .put(fingerprint, ());
            warn!(
                "Tx {} signature is invalid (fingerprint {:?})",
                &tx.id(),
                fingerprint
            );
            debug!(
                target = "invalid_tx_header_json",
                "Invalid tx: {:#}",
                &serde_json::to_string(&tx).unwrap_or_else(|e| format!(
                    // fallback to debug printing the header
                    "error serializing block header: {}\n{:?}",
                    &e, &tx
                ))
            );
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
        state.recent_invalid_tx.put(tx_id, ());
        state.recent_valid_tx.pop(&tx_id);
    }

    // Helper to get the canonical chain and latest height
    fn get_latest_block_height(&self) -> Result<u64, TxIngressError> {
        // TODO: `get_canonical_chain` clones the entire canonical chain, we can make do with a ref here
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
    /// bounded map with manual capacity enforcement
    pub valid_submit_ledger_tx: BTreeMap<H256, DataTransactionHeader>,
    pub max_submit_txs: usize,
    /// bounded map with manual capacity enforcement
    pub valid_commitment_tx: BTreeMap<Address, Vec<CommitmentTransaction>>,
    pub max_commitment_addresses: usize,
    pub max_commitments_per_address: usize,
    /// The miner's signer instance, used to sign ingress proofs
    pub recent_invalid_tx: LruCache<H256, ()>,
    /// Tracks recent invalid payload fingerprints (e.g., keccak(prehash + signature)) for signature-invalid payloads
    /// Prevents poisoning legitimate txids via mismatched id/signature pairs
    pub recent_invalid_payload_fingerprints: LruCache<H256, ()>,
    /// Tracks recent valid txids from either data or commitment
    pub recent_valid_tx: LruCache<H256, ()>,
    /// Tracks recently processed chunk hashes to prevent re-gossip
    pub recent_valid_chunks: LruCache<ChunkPathHash, ()>,
    /// LRU caches for out of order gossip data
    pub pending_chunks: LruCache<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    pub pending_pledges: LruCache<Address, LruCache<IrysTransactionId, CommitmentTransaction>>,
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(config: &MempoolConfig) -> MempoolState {
    let max_pending_chunk_items = config.max_pending_chunk_items;
    let max_pending_pledge_items = config.max_pending_pledge_items;
    MempoolState {
        valid_submit_ledger_tx: BTreeMap::new(),
        max_submit_txs: config.max_valid_submit_txs,
        valid_commitment_tx: BTreeMap::new(),
        max_commitment_addresses: config.max_valid_commitment_addresses,
        max_commitments_per_address: config.max_commitments_per_address,
        recent_invalid_tx: LruCache::new(NonZeroUsize::new(config.max_invalid_items).unwrap()),
        recent_invalid_payload_fingerprints: LruCache::new(
            NonZeroUsize::new(config.max_invalid_items).unwrap(),
        ),
        recent_valid_tx: LruCache::new(NonZeroUsize::new(config.max_valid_items).unwrap()),
        recent_valid_chunks: LruCache::new(NonZeroUsize::new(config.max_valid_chunks).unwrap()),
        pending_chunks: LruCache::new(NonZeroUsize::new(max_pending_chunk_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
    }
}

impl MempoolState {
    /// Insert data tx with bounds enforcement.
    /// Evicts lowest fee tx if at capacity.
    /// Returns error only if capacity is exceeded and no transaction can be evicted.
    pub fn bounded_insert_data_tx(
        &mut self,
        tx: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        use std::collections::btree_map::Entry;
        // If tx already exists we still update it.
        // the new entry might have the `is_promoted` flag set on it, which is needed for correct promotion logic
        if let Entry::Occupied(mut value) = self.valid_submit_ledger_tx.entry(tx.id) {
            value.insert(tx);
            return Ok(());
        }

        // If at capacity, evict lowest fee tx
        if self.valid_submit_ledger_tx.len() >= self.max_submit_txs {
            if let Some((evict_id, evicted_fee)) = self.find_lowest_fee_data_tx() {
                let new_fee = tx.user_fee();
                // Only evict if new tx has higher fee
                if new_fee <= evicted_fee {
                    warn!(
                        new.tx_id = ?tx.id,
                        new.fee = ?new_fee,
                        lowest.fee = ?evicted_fee,
                        "Rejecting lower-fee tx: mempool full"
                    );
                    return Err(TxIngressError::MempoolFull(
                        "Mempool full and new transaction fee too low".to_string(),
                    ));
                }

                self.valid_submit_ledger_tx.remove(&evict_id);
                warn!(
                    evicted.tx_id = ?evict_id,
                    evicted.fee = ?evicted_fee,
                    new.tx_id = ?tx.id,
                    new.fee = ?new_fee,
                    "Mempool full: evicted lowest fee data tx"
                );
            } else {
                return Err(TxIngressError::MempoolFull(
                    "Mempool full and no evictable data tx found".to_string(),
                ));
            }
        }

        self.valid_submit_ledger_tx.insert(tx.id, tx);
        Ok(())
    }

    /// Find lowest fee data transaction for eviction.
    /// Returns (tx_id, fee) tuple.
    fn find_lowest_fee_data_tx(&self) -> Option<(H256, U256)> {
        self.valid_submit_ledger_tx
            .iter()
            .min_by_key(|(_, tx)| tx.user_fee())
            .map(|(id, tx)| (*id, tx.user_fee()))
    }

    /// Insert commitment tx with bounds enforcement.
    /// Enforces per-address limit and global address limit.
    /// Rejects new commitments when per-address limit is exceeded (preserves existing commitments).
    /// Evicts lowest-value addresses when global address limit is exceeded.
    pub fn bounded_insert_commitment_tx(
        &mut self,
        tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let address = tx.signer;

        // Check for duplicate tx.id - if already exists, just return Ok()
        if let Some(existing_txs) = self.valid_commitment_tx.get(&address) {
            if existing_txs.iter().any(|t| t.id == tx.id) {
                return Ok(()); // Duplicate, already have this commitment
            }
        }

        // Check if we need to create a new address entry
        let address_exists = self.valid_commitment_tx.contains_key(&address);

        // If address doesn't exist and we're at global address limit, evict lowest value address
        if !address_exists && self.valid_commitment_tx.len() >= self.max_commitment_addresses {
            if let Some((evict_address, evict_total_value)) = self.find_lowest_value_address() {
                let new_value = tx.total_cost();

                // Only evict if new commitment has higher value than the address being evicted
                if new_value <= evict_total_value {
                    warn!(
                        new.address = ?address,
                        new.value = ?new_value,
                        evict.address = ?evict_address,
                        evict.total_value = ?evict_total_value,
                        "Rejecting new commitment: value not higher than lowest existing address"
                    );
                    return Err(TxIngressError::MempoolFull(format!(
                        "Mempool address limit reached. New commitment value {} not higher than lowest address value {}",
                        new_value,
                        evict_total_value
                    )));
                }

                let evicted_txs = self.valid_commitment_tx.remove(&evict_address);
                warn!(
                    evicted.address = ?evict_address,
                    evicted.total_value = ?evict_total_value,
                    evicted.count = ?evicted_txs.as_ref().map(std::vec::Vec::len),
                    new.address = ?address,
                    new.value = ?new_value,
                    "Mempool address limit reached: evicted lowest value address"
                );
            } else {
                return Err(TxIngressError::MempoolFull(
                    "Mempool address limit reached and no evictable address found".to_string(),
                ));
            }
        }

        // Get or create vec for this address
        let txs = self.valid_commitment_tx.entry(address).or_default();

        // Check if address vec is at capacity - reject new commitment rather than evicting old ones
        // This preserves the stake/pledge lifecycle and prevents breaking protocol invariants
        if txs.len() >= self.max_commitments_per_address {
            warn!(
                address = ?address,
                current_count = txs.len(),
                max_allowed = self.max_commitments_per_address,
                new.tx_id = ?tx.id,
                "Address commitment pool full: rejecting new commitment"
            );
            return Err(TxIngressError::MempoolFull(format!(
                "Address {} has reached maximum commitments limit ({}/{})",
                address,
                txs.len(),
                self.max_commitments_per_address
            )));
        }

        txs.push(tx.clone());
        Ok(())
    }

    /// Find address with lowest total commitment value for eviction.
    /// Returns (Address, total_value) tuple.
    fn find_lowest_value_address(&self) -> Option<(Address, U256)> {
        self.valid_commitment_tx
            .iter()
            .map(|(addr, txs)| {
                let total = txs
                    .iter()
                    .map(irys_types::CommitmentTransaction::total_cost)
                    .fold(U256::zero(), irys_types::U256::saturating_add);
                (*addr, total)
            })
            .min_by_key(|(_, total)| *total)
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
    /// Mempool is at capacity and cannot accept new transactions
    #[error("Mempool is at capacity: {0}")]
    MempoolFull(String),
    /// Catch-all variant for other errors.
    #[error("Transaction ingress error: {0}")]
    Other(String),
    /// Transaction has encountered a fee calculation issue
    #[error("Transaction misaligned funds: {0}")]
    FundMisalignment(String),
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
    /// The ingress proof is anchored to an unknown/expired anchor
    #[error("Invalid anchor: {0}")]
    InvalidAnchor(BlockHash),
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
        let initial_stake_and_pledge_whitelist = config
            .node_config
            .initial_stake_and_pledge_whitelist
            .clone();

        let handle = runtime_handle.spawn(
            async move {
                let mempool_state = Arc::new(RwLock::new(mempool_state));
                let pledge_provider = MempoolPledgeProvider::new(
                    mempool_state.clone(),
                    block_tree_read_guard.clone(),
                );

                let mut stake_and_pledge_whitelist = HashSet::new();
                stake_and_pledge_whitelist.extend(initial_stake_and_pledge_whitelist);

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
                        stake_and_pledge_whitelist,
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

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
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

#[cfg(test)]
mod bounded_mempool_tests {
    use super::*;
    use irys_types::{
        CommitmentTransactionV1, CommitmentType, DataLedger, DataTransactionHeaderV1, IrysSignature,
    };

    // ========================================================================
    // Test Helpers
    // ========================================================================

    /// Creates a test data transaction with specified fee
    fn create_test_data_tx(fee: u64) -> DataTransactionHeader {
        DataTransactionHeader::V1(DataTransactionHeaderV1 {
            id: H256::random(),
            anchor: H256::zero(),
            signer: Address::random(),
            data_root: H256::random(),
            data_size: 1024,
            header_size: 0,
            term_fee: U256::from(fee),
            perm_fee: Some(U256::from(100)),
            ledger_id: DataLedger::Publish as u32,
            bundle_format: Some(0),
            promoted_height: None,
            signature: IrysSignature::default(),
            chain_id: 1,
        })
    }

    /// Creates a test commitment transaction with specified signer and value
    fn create_test_commitment_tx(signer: Address, value: u64) -> CommitmentTransaction {
        CommitmentTransaction::V1(CommitmentTransactionV1 {
            id: H256::random(), // Random ID for testing
            anchor: H256::zero(),
            signer,
            signature: IrysSignature::default(),
            fee: 100,
            value: U256::from(value),
            commitment_type: CommitmentType::Stake,
            chain_id: 1,
        })
    }

    /// Creates a test mempool state with specified capacities
    fn create_test_mempool_state(
        max_data: usize,
        max_addresses: usize,
        max_per_address: usize,
    ) -> MempoolState {
        MempoolState {
            valid_submit_ledger_tx: BTreeMap::new(),
            max_submit_txs: max_data,
            valid_commitment_tx: BTreeMap::new(),
            max_commitment_addresses: max_addresses,
            max_commitments_per_address: max_per_address,
            recent_invalid_tx: LruCache::new(NonZeroUsize::new(100).unwrap()),
            recent_invalid_payload_fingerprints: LruCache::new(NonZeroUsize::new(100).unwrap()),
            recent_valid_tx: LruCache::new(NonZeroUsize::new(100).unwrap()),
            recent_valid_chunks: LruCache::new(NonZeroUsize::new(100).unwrap()),
            pending_chunks: LruCache::new(NonZeroUsize::new(10).unwrap()),
            pending_pledges: LruCache::new(NonZeroUsize::new(10).unwrap()),
        }
    }

    // ========================================================================
    // Data Transaction Capacity Tests
    // ========================================================================

    #[test]
    fn test_data_tx_evicts_lowest_fee_when_full() {
        // Setup: Fill mempool to capacity with 3 txs
        let mut state = create_test_mempool_state(3, 10, 10);

        let tx1 = create_test_data_tx(100); // Lowest fee - should be evicted
        let tx2 = create_test_data_tx(200);
        let tx3 = create_test_data_tx(300);
        let tx_new = create_test_data_tx(250); // Higher than lowest

        state.bounded_insert_data_tx(tx1.clone()).unwrap();
        state.bounded_insert_data_tx(tx2.clone()).unwrap();
        state.bounded_insert_data_tx(tx3.clone()).unwrap();

        assert_eq!(state.valid_submit_ledger_tx.len(), 3);

        // Act: Insert new tx with fee 250 (should evict tx1 with fee 100)
        let result = state.bounded_insert_data_tx(tx_new.clone());

        // Assert: Insertion succeeded
        assert!(result.is_ok(), "Should successfully evict lowest fee tx");

        // Assert: Mempool still at capacity
        assert_eq!(state.valid_submit_ledger_tx.len(), 3);

        // Assert: Lowest fee tx evicted
        assert!(!state.valid_submit_ledger_tx.contains_key(&tx1.id));

        // Assert: New tx inserted
        assert!(state.valid_submit_ledger_tx.contains_key(&tx_new.id));

        // Assert: Other txs preserved
        assert!(state.valid_submit_ledger_tx.contains_key(&tx2.id));
        assert!(state.valid_submit_ledger_tx.contains_key(&tx3.id));
    }

    #[test]
    fn test_data_tx_rejects_lower_fee_when_full() {
        // Setup: Fill mempool to capacity
        let mut state = create_test_mempool_state(3, 10, 10);

        let tx1 = create_test_data_tx(100);
        let tx2 = create_test_data_tx(200);
        let tx3 = create_test_data_tx(300);
        let tx_low_fee = create_test_data_tx(50); // Lower than all existing

        state.bounded_insert_data_tx(tx1.clone()).unwrap();
        state.bounded_insert_data_tx(tx2.clone()).unwrap();
        state.bounded_insert_data_tx(tx3.clone()).unwrap();

        // Act: Try to insert tx with lower fee than all existing
        let result = state.bounded_insert_data_tx(tx_low_fee.clone());

        // Assert: Insertion rejected
        assert!(matches!(result, Err(TxIngressError::MempoolFull(_))));

        // Assert: Mempool unchanged
        assert_eq!(state.valid_submit_ledger_tx.len(), 3);
        assert!(state.valid_submit_ledger_tx.contains_key(&tx1.id));
        assert!(state.valid_submit_ledger_tx.contains_key(&tx2.id));
        assert!(state.valid_submit_ledger_tx.contains_key(&tx3.id));
        assert!(!state.valid_submit_ledger_tx.contains_key(&tx_low_fee.id));
    }

    // ========================================================================
    // Commitment Transaction Per-Address Limit Tests
    // ========================================================================

    #[test]
    fn test_commitment_tx_rejects_when_address_limit_reached() {
        // Setup: Create state with max 3 commitments per address
        let mut state = create_test_mempool_state(10, 10, 3);

        let address = Address::random();
        let tx1 = create_test_commitment_tx(address, 100);
        let tx2 = create_test_commitment_tx(address, 200);
        let tx3 = create_test_commitment_tx(address, 300);
        let tx4 = create_test_commitment_tx(address, 400); // Should be rejected

        // Act: Insert 3 commitments (fill address limit)
        state.bounded_insert_commitment_tx(&tx1).unwrap();
        state.bounded_insert_commitment_tx(&tx2).unwrap();
        state.bounded_insert_commitment_tx(&tx3).unwrap();

        // Assert: 3 commitments stored
        assert_eq!(state.valid_commitment_tx.get(&address).unwrap().len(), 3);

        // Act: Try to insert 4th commitment for same address
        let result = state.bounded_insert_commitment_tx(&tx4);

        // Assert: Rejected with MempoolFull error
        assert!(matches!(result, Err(TxIngressError::MempoolFull(_))));

        // Assert: Original 3 commitments preserved (no eviction)
        let txs = state.valid_commitment_tx.get(&address).unwrap();
        assert_eq!(txs.len(), 3);
        assert!(txs.iter().any(|t| t.id == tx1.id));
        assert!(txs.iter().any(|t| t.id == tx2.id));
        assert!(txs.iter().any(|t| t.id == tx3.id));
        assert!(!txs.iter().any(|t| t.id == tx4.id));
    }

    #[test]
    fn test_commitment_tx_different_addresses_independent_limits() {
        // Setup: Max 2 commitments per address
        let mut state = create_test_mempool_state(10, 10, 2);

        let addr_a = Address::random();
        let addr_b = Address::random();

        let tx_a1 = create_test_commitment_tx(addr_a, 100);
        let tx_a2 = create_test_commitment_tx(addr_a, 200);
        let tx_a3 = create_test_commitment_tx(addr_a, 300);

        let tx_b1 = create_test_commitment_tx(addr_b, 100);
        let tx_b2 = create_test_commitment_tx(addr_b, 200);
        let tx_b3 = create_test_commitment_tx(addr_b, 300);

        // Act: Fill both addresses to limit
        state.bounded_insert_commitment_tx(&tx_a1).unwrap();
        state.bounded_insert_commitment_tx(&tx_a2).unwrap();
        state.bounded_insert_commitment_tx(&tx_b1).unwrap();
        state.bounded_insert_commitment_tx(&tx_b2).unwrap();

        // Assert: Both addresses have 2 commitments
        assert_eq!(state.valid_commitment_tx.get(&addr_a).unwrap().len(), 2);
        assert_eq!(state.valid_commitment_tx.get(&addr_b).unwrap().len(), 2);

        // Act: Try to add 3rd to each address
        let result_a3 = state.bounded_insert_commitment_tx(&tx_a3);
        let result_b3 = state.bounded_insert_commitment_tx(&tx_b3);

        // Assert: Both rejected independently
        assert!(matches!(result_a3, Err(TxIngressError::MempoolFull(_))));
        assert!(matches!(result_b3, Err(TxIngressError::MempoolFull(_))));

        // Assert: Limits still at 2 per address
        assert_eq!(state.valid_commitment_tx.get(&addr_a).unwrap().len(), 2);
        assert_eq!(state.valid_commitment_tx.get(&addr_b).unwrap().len(), 2);
    }

    // ========================================================================
    // Commitment Transaction Global Address Limit Tests
    // ========================================================================

    #[test]
    fn test_commitment_tx_evicts_lowest_value_address_when_global_limit_reached() {
        // Setup: Max 3 addresses globally
        let mut state = create_test_mempool_state(10, 3, 10);

        let addr_a = Address::random();
        let addr_b = Address::random();
        let addr_c = Address::random();
        let addr_d = Address::random();

        // Create commitments with different total values per address
        let tx_a = create_test_commitment_tx(addr_a, 100); // Lowest total value
        let tx_b1 = create_test_commitment_tx(addr_b, 200);
        let tx_b2 = create_test_commitment_tx(addr_b, 300); // Total: 500
        let tx_c = create_test_commitment_tx(addr_c, 300);
        let tx_d = create_test_commitment_tx(addr_d, 200); // Should evict addr_a

        // Act: Fill 3 addresses
        state.bounded_insert_commitment_tx(&tx_a).unwrap();
        state.bounded_insert_commitment_tx(&tx_b1).unwrap();
        state.bounded_insert_commitment_tx(&tx_b2).unwrap();
        state.bounded_insert_commitment_tx(&tx_c).unwrap();

        assert_eq!(state.valid_commitment_tx.len(), 3); // 3 addresses

        // Act: Insert commitment for 4th address (should evict addr_a with lowest value)
        let result = state.bounded_insert_commitment_tx(&tx_d);

        // Assert: Insertion succeeded
        assert!(result.is_ok());

        // Assert: Still 3 addresses total
        assert_eq!(state.valid_commitment_tx.len(), 3);

        // Assert: Lowest value address (addr_a) evicted
        assert!(!state.valid_commitment_tx.contains_key(&addr_a));

        // Assert: New address inserted
        assert!(state.valid_commitment_tx.contains_key(&addr_d));

        // Assert: Other addresses preserved
        assert!(state.valid_commitment_tx.contains_key(&addr_b));
        assert!(state.valid_commitment_tx.contains_key(&addr_c));

        // Assert: addr_b still has both commitments
        assert_eq!(state.valid_commitment_tx.get(&addr_b).unwrap().len(), 2);
    }

    #[test]
    fn test_commitment_tx_updates_existing_address_no_global_limit_check() {
        // Setup: Fill global address limit
        let mut state = create_test_mempool_state(10, 3, 10);

        let addr_a = Address::random();
        let addr_b = Address::random();
        let addr_c = Address::random();

        state
            .bounded_insert_commitment_tx(&create_test_commitment_tx(addr_a, 100))
            .unwrap();
        state
            .bounded_insert_commitment_tx(&create_test_commitment_tx(addr_b, 200))
            .unwrap();
        state
            .bounded_insert_commitment_tx(&create_test_commitment_tx(addr_c, 300))
            .unwrap();

        assert_eq!(state.valid_commitment_tx.len(), 3);

        // Act: Add another commitment to existing address
        let tx_a2 = create_test_commitment_tx(addr_a, 150);
        let result = state.bounded_insert_commitment_tx(&tx_a2);

        // Assert: Succeeds without triggering global limit check
        assert!(result.is_ok());

        // Assert: Still 3 addresses
        assert_eq!(state.valid_commitment_tx.len(), 3);

        // Assert: Address A now has 2 commitments
        assert_eq!(state.valid_commitment_tx.get(&addr_a).unwrap().len(), 2);
    }
}
