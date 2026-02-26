pub mod commitment_txs;
pub mod data_txs;
pub mod facade;
pub mod lifecycle;
pub mod pledge_provider;
pub mod types;

pub use facade::*;
pub use types::*;

use crate::block_tree_service::ReorgEvent;
use crate::chunk_ingress_service::ChunkIngressState;
use crate::pledge_provider::MempoolPledgeProvider;
use crate::services::ServiceSenders;
use crate::shadow_tx_generator::PublishLedgerWithTxs;
use crate::{MempoolReadGuard, TxMetadata};
use irys_database::db::IrysDatabaseExt as _;
use irys_domain::{BlockTreeReadGuard, CommitmentSnapshotStatus, get_atomic_file};
use irys_reth_node_bridge::{IrysRethNodeAdapter, ext::IrysRethRpcTestContextExt as _};
use irys_storage::RecoveredMempoolState;
use irys_types::CommitmentTypeV2;
use irys_types::{
    BoundedFee, Config, H256, IrysTransactionCommon, IrysTransactionId, NodeConfig, SealedBlock,
    U256, app_state::DatabaseProvider,
};
use irys_types::{
    CommitmentTransaction, CommitmentValidationError, DataTransactionHeader, IrysAddress,
    MempoolConfig,
};
use irys_types::{TokioServiceHandle, Traced, TxKnownStatus};
use lru::LruCache;
use reth::rpc::types::BlockId;
use reth::tasks::TaskExecutor;
use reth::tasks::shutdown::Shutdown;
use std::fmt::Display;
use std::fs;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{RwLock, Semaphore, broadcast, mpsc::UnboundedReceiver, oneshot};
use tokio::time::MissedTickBehavior;
use tracing::{Instrument as _, Span, debug, error, info, instrument, warn};

/// Public helper to validate that a commitment transaction is sufficiently funded.
/// Checks the current balance of the signer via the provided reth adapter and ensures it
/// covers the total cost (value + fee) of the transaction.
#[inline]
#[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
pub async fn validate_funding(
    reth_adapter: &IrysRethNodeAdapter,
    commitment_tx: &irys_types::CommitmentTransaction,
    parent_evm_block_id: Option<BlockId>,
) -> Result<(), TxIngressError> {
    // Fetch the current balance of the signer
    let balance: irys_types::U256 = reth_adapter
        .rpc
        .get_balance_irys_canonical_and_pending(commitment_tx.signer(), parent_evm_block_id)
        .await
        .map_err(|e| {
            tracing::error!(
                tx.id = %commitment_tx.id(),
                tx.signer = %commitment_tx.signer(),
                tx.error = %e,
                "Failed to fetch balance for commitment tx"
            );
            TxIngressError::BalanceFetchError {
                address: commitment_tx.signer().to_string(),
                reason: e.to_string(),
            }
        })?;

    let required = commitment_tx.total_cost();

    if balance < required {
        tracing::warn!(
            tx.id = %commitment_tx.id(),
            account.balance = %balance,
            tx.required_balance = %required,
            tx.signer = %commitment_tx.signer(),
            "Insufficient balance for commitment tx"
        );
        return Err(TxIngressError::Unfunded(commitment_tx.id()));
    }

    tracing::debug!(
        tx.id = %commitment_tx.id(),
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
#[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
pub async fn validate_commitment_transaction(
    reth_adapter: &IrysRethNodeAdapter,
    consensus: &irys_types::ConsensusConfig,
    commitment_tx: &irys_types::CommitmentTransaction,
    parent_evm_block_id: Option<BlockId>,
) -> Result<(), TxIngressError> {
    debug!(
        tx.id = ?commitment_tx.id(),
        tx.signer = ?commitment_tx.signer(),
        "Validating commitment transaction"
    );
    // Fee
    commitment_tx.validate_fee(consensus).map_err(|e| {
        warn!(
            tx.id = ?commitment_tx.id(),
            tx.signer = ?commitment_tx.signer(),
            tx.error = ?e,
            "Commitment tx fee validation failed"
        );
        TxIngressError::from(e)
    })?;

    // Funding
    validate_funding(reth_adapter, commitment_tx, parent_evm_block_id)
        .await
        .map_err(|e| {
            warn!(
                tx.id = ?commitment_tx.id(),
                tx.signer = ?commitment_tx.signer(),
                tx.error = ?e,
                "Commitment tx funding validation failed"
            );
            e
        })?;

    // Value
    commitment_tx.validate_value(consensus).map_err(|e| {
        warn!(
            tx.id = ?commitment_tx.id(),
            tx.signer = ?commitment_tx.signer(),
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
    /// Pledge provider for commitment transaction validation
    pub pledge_provider: MempoolPledgeProvider,
    message_handler_semaphore: Arc<Semaphore>,
    /// Shared state handle for reading chunk ingress pending count
    pub chunk_ingress_state: ChunkIngressState,
}

/// Messages that the Mempool Service handler supports
#[derive(Debug)]
pub enum MempoolServiceMessage {
    /// Block Confirmed, read publish txs from block. Overwrite copies in mempool with proof
    BlockConfirmed(Arc<SealedBlock>),
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
    GetState(oneshot::Sender<AtomicMempoolState>),
    /// Remove the set of txids from any blocklists (recent_invalid_txs)
    RemoveFromBlacklist(Vec<H256>, oneshot::Sender<()>),
    UpdateStakeAndPledgeWhitelist(HashSet<IrysAddress>, oneshot::Sender<()>),
    CloneStakeAndPledgeWhitelist(oneshot::Sender<HashSet<IrysAddress>>),
    /// Obtain a read guard with broad access to mempool state.
    /// Avoid holding the guard across long‑running operations to prevent
    /// reducing mempool write throughput.
    GetReadGuard(oneshot::Sender<MempoolReadGuard>),
}

impl MempoolServiceMessage {
    /// Returns the variant name as a static string for tracing/logging purposes
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::BlockConfirmed(_) => "BlockConfirmed",
            Self::CommitmentTxExists(_, _) => "CommitmentTxExists",
            Self::IngestCommitmentTxFromApi(_, _) => "IngestCommitmentTxFromApi",
            Self::IngestCommitmentTxFromGossip(_, _) => "IngestCommitmentTxFromGossip",
            Self::DataTxExists(_, _) => "DataTxExists",
            Self::IngestDataTxFromApi(_, _) => "IngestDataTxFromApi",
            Self::IngestDataTxFromGossip(_, _) => "IngestDataTxFromGossip",
            Self::GetState(_) => "GetState",
            Self::RemoveFromBlacklist(_, _) => "RemoveFromBlacklist",
            Self::UpdateStakeAndPledgeWhitelist(_, _) => "UpdateStakeAndPledgeWhitelist",
            Self::CloneStakeAndPledgeWhitelist(_) => "CloneStakeAndPledgeWhitelist",
            Self::GetReadGuard(_) => "GetReadGuard",
        }
    }
}

impl Inner {
    #[tracing::instrument(level = "trace", skip_all, err)]
    /// handle inbound MempoolServiceMessage and send oneshot responses where required to do so
    pub async fn handle_message(&self, msg: MempoolServiceMessage) -> eyre::Result<()> {
        match msg {
            MempoolServiceMessage::BlockConfirmed(sealed_block) => {
                let block_hash = sealed_block.header().block_hash;
                let block_height = sealed_block.header().height;
                if let Err(e) = self.handle_block_confirmed_message(sealed_block).await {
                    tracing::error!(
                        "Failed to handle block confirmed message for block {} (height {}): {:#}",
                        block_hash,
                        block_height,
                        e
                    );
                }
            }
            MempoolServiceMessage::IngestCommitmentTxFromApi(commitment_tx, response) => {
                let response_message = self
                    .handle_ingress_commitment_tx_message_api(commitment_tx)
                    .instrument(tracing::info_span!(
                        "mempool.ingest_commitment_tx",
                        source = "api"
                    ))
                    .await;
                if let Err(e) = response.send(response_message) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::IngestCommitmentTxFromGossip(commitment_tx, response) => {
                let response_message = self
                    .handle_ingress_commitment_tx_message_gossip(commitment_tx)
                    .instrument(tracing::info_span!(
                        "mempool.ingest_commitment_tx",
                        source = "gossip"
                    ))
                    .await;
                if let Err(e) = response.send(response_message) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::DataTxExists(txid, response) => {
                let response_value = self.handle_data_tx_exists_message(txid).await;
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
                let response_value = self
                    .handle_data_tx_ingress_message_api(tx)
                    .instrument(tracing::info_span!(
                        "mempool.ingest_data_tx",
                        source = "api"
                    ))
                    .await;
                if let Err(e) = response.send(response_value) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::IngestDataTxFromGossip(tx, response) => {
                let response_value = self
                    .handle_data_tx_ingress_message_gossip(tx)
                    .instrument(tracing::info_span!(
                        "mempool.ingest_data_tx",
                        source = "gossip"
                    ))
                    .await;
                if let Err(e) = response.send(response_value) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }

            MempoolServiceMessage::GetState(response) => {
                if let Err(e) = response
                    .send(self.mempool_state.clone())
                    .inspect_err(|e| tracing::error!("response.send() error: {:?}", e))
                {
                    tracing::error!("response.send() error: {:?}", e);
                }
            }
            MempoolServiceMessage::RemoveFromBlacklist(tx_ids, response) => {
                let response_value = self.remove_from_blacklists(tx_ids).await;
                if let Err(e) = response.send(response_value) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::UpdateStakeAndPledgeWhitelist(new_entries, response) => {
                self.extend_stake_and_pledge_whitelist(new_entries).await;
                if let Err(e) = response.send(()) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::CloneStakeAndPledgeWhitelist(tx) => {
                let whitelist = self.get_stake_and_pledge_whitelist_cloned().await;
                if let Err(e) = tx.send(whitelist) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
            MempoolServiceMessage::GetReadGuard(tx) => {
                let guard = MempoolReadGuard::new(self.mempool_state.clone());
                if let Err(e) = tx.send(guard) {
                    tracing::error!("response.send() error: {:?}", e);
                };
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(tx.count = tx_ids.len()))]
    async fn remove_from_blacklists(&self, tx_ids: Vec<H256>) {
        self.mempool_state.remove_blacklisted_txids(&tx_ids).await;
    }

    // Helper to validate anchor
    // this takes in an IrysTransaction and validates the anchor
    // if the anchor is valid, returns anchor block height
    #[instrument(level = "trace", skip_all, fields(tx.id = ?tx.id(), anchor = %tx.anchor()))]
    pub async fn validate_tx_anchor(
        &self,
        tx: &impl IrysTransactionCommon,
    ) -> Result<u64, TxIngressError> {
        let tx_id = tx.id();
        let anchor = tx.anchor();

        let latest_height =
            crate::anchor_validation::get_latest_block_height(&self.block_tree_read_guard)?;

        // let anchor_height = self.get_anchor_height(tx_id, anchor).await?;

        let anchor_height = match crate::anchor_validation::get_anchor_height(
            &self.block_tree_read_guard,
            &self.irys_db,
            anchor,
            false, /* does not need to be canonical */
        )
        .map_err(|e| {
            TxIngressError::DatabaseError(format!(
                "Error getting anchor height for {}: {}",
                anchor, e
            ))
        })? {
            Some(height) => height,
            None => {
                return Err(TxIngressError::InvalidAnchor(anchor));
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
            self.mempool_state.mark_tx_as_invalid(
                tx_id,
                format!(
                    "Invalid anchor value for tx {tx_id} - anchor {anchor}@{anchor_height} is too old ({anchor_height}<{min_anchor_height})"
                )
            ).await;

            return Err(TxIngressError::InvalidAnchor(anchor));
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn persist_mempool_to_disk(&self) -> eyre::Result<()> {
        let base_path = self.config.node_config.mempool_dir();

        let commitment_tx_path = base_path.join("commitment_tx");
        fs::create_dir_all(commitment_tx_path.clone())
            .expect("to create the mempool/commitment_tx dir");
        let commitment_hash_map = self.mempool_state.get_all_commitment_tx().await;
        for tx in commitment_hash_map.values() {
            // Create a filepath for this transaction
            let tx_path = commitment_tx_path.join(format!("{}.json", tx.id()));

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

    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn restore_mempool_from_disk(&self) {
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

        self.mempool_state.wipe_blacklists().await;
    }

    /// After restoring the mempool from disk, reconstruct metadata fields (included_height,
    /// promoted_height) from the database. The `#[serde(skip)]` on `DataTransactionMetadata`
    /// means these fields are lost during serialization. The DB is authoritative since
    /// `BlockMigrationService` persists them at confirmation time.
    pub async fn reconstruct_metadata_from_db(&self) {
        let mut state = self.mempool_state.0.write().await;
        let mut reconstructed = 0_u64;
        for (txid, tx_header) in state.valid_submit_ledger_tx.iter_mut() {
            let db_meta = match self.irys_db.view_eyre(|tx| {
                irys_database::get_data_tx_metadata(tx, txid).map_err(|e| eyre::eyre!("{:?}", e))
            }) {
                Ok(meta) => meta,
                Err(e) => {
                    tracing::error!(tx.id = %txid, "Failed to read tx metadata from DB: {e}");
                    None
                }
            };
            if let Some(meta) = db_meta {
                let mut changed = false;
                if meta.included_height.is_some() {
                    tx_header.metadata_mut().included_height = meta.included_height;
                    changed = true;
                }
                if meta.promoted_height.is_some() {
                    tx_header.metadata_mut().promoted_height = meta.promoted_height;
                    changed = true;
                }
                if changed {
                    reconstructed += 1;
                }
            }
        }
        if reconstructed > 0 {
            tracing::info!(
                reconstructed,
                "Reconstructed metadata (included_height, promoted_height) from DB for mempool txs"
            );
        }
    }

    async fn extend_stake_and_pledge_whitelist(&self, new_entries: HashSet<IrysAddress>) {
        self.mempool_state
            .extend_stake_and_pledge_whitelist(new_entries)
            .await;
    }

    async fn get_stake_and_pledge_whitelist_cloned(&self) -> HashSet<IrysAddress> {
        self.mempool_state
            .get_stake_and_pledge_whitelist_cloned()
            .await
    }
}

/// Validate a transaction's cryptographic signature.
/// Returns Ok(()) if valid, Err(InvalidSignature) if invalid.
/// Does NOT mark the fingerprint as invalid — callers must do that themselves.
#[instrument(level = "trace", skip_all, fields(tx.id = ?tx.id()))]
pub(crate) fn validate_tx_signature<
    T: irys_types::versioning::Signable + IrysTransactionCommon + std::fmt::Debug + serde::Serialize,
>(
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
        let fingerprint = tx.fingerprint();
        warn!(
            "Tx {} signature is invalid (fingerprint {:?})",
            &tx.id(),
            fingerprint
        );
        debug!(
            target = "invalid_tx_header_json",
            "Invalid tx: {:#}",
            &serde_json::to_string(&tx)
                .unwrap_or_else(|e| format!("error serializing block header: {}\n{:?}", &e, &tx))
        );
        Err(TxIngressError::InvalidSignature(tx.signer()))
    }
}

/// Promotion readiness evaluation outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionStatus {
    AlreadyPromoted,
    MissingSubmitInclusion,
    InsufficientProofs,
    Ready,
}

/// Mempool state. All methods on this structure are quick utility methods. No function acquires
/// more than one lock, and no function makes a call to any other function of the
/// AtomicMempoolState structure, eliminating the possibility of deadlocks. Although this structure
/// has private read and write methods, these should not be used outside the AtomicMempoolState
/// under any condition.
#[derive(Debug, Clone)]
pub struct AtomicMempoolState(Arc<RwLock<MempoolState>>);

impl AtomicMempoolState {
    pub fn new(inner: MempoolState) -> Self {
        Self(Arc::new(RwLock::new(inner)))
    }

    pub async fn remove_blacklisted_txids(&self, tx_ids: &[H256]) {
        let mut state = self.write().await;
        for tx_id in tx_ids {
            state.recent_invalid_tx.pop(tx_id);
        }
    }

    /// Marks a given tx as invalid, adding it's ID to `recent_invalid_tx` and removing it from `recent_valid_tx`
    pub async fn mark_tx_as_invalid(&self, tx_id: IrysTransactionId, err_reason: impl ToString) {
        self.write().await.mark_tx_as_invalid(tx_id, err_reason);
    }

    /// Batch-removes expired data txs under a single write lock.
    /// Each entry is (tx_id, anchor) — the anchor is used for the InvalidAnchor error reason.
    pub async fn batch_prune_data_txs(&self, expired: &[(H256, H256)]) {
        self.write().await.batch_prune_data_txs(expired);
    }

    /// Batch-removes expired commitment txs under a single write lock.
    /// Each entry is (tx_id, anchor) — the anchor is used for the InvalidAnchor error reason.
    pub async fn batch_prune_commitment_txs(&self, expired: &[(H256, H256)]) {
        self.write().await.batch_prune_commitment_txs(expired);
    }

    pub async fn sorted_commitments(&self) -> Vec<CommitmentTransaction> {
        let mempool_state_guard = self.read().await;

        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter().cloned())
            .collect::<Vec<_>>()
    }

    /// Get specific commitment transactions by their IDs from the mempool
    ///
    /// This searches both:
    /// - `valid_commitment_tx`: Validated commitment transactions organized by address
    /// - `pending_pledges`: Out-of-order pledge transactions waiting for dependencies
    ///
    /// Returns a HashMap containing only the requested transactions that were found.
    ///
    /// Complexity: O(n + m) where n is the number of requested IDs and m is the total
    /// number of transactions in the mempool.
    #[must_use]
    pub async fn get_commitment_txs(
        &self,
        commitment_tx_ids: &[IrysTransactionId],
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        const PRESUMED_PLEDGES_PER_ACCOUNT: usize = 4;
        let mempool_state_guard = self.read().await;

        // Build lookup map of ALL transactions in mempool - O(m)
        let mut all_txs = HashMap::with_capacity(
            mempool_state_guard.valid_commitment_tx.len()
                + mempool_state_guard.pending_pledges.len() * PRESUMED_PLEDGES_PER_ACCOUNT,
        );

        // Collect from valid_commitment_tx
        for txs in mempool_state_guard.valid_commitment_tx.values() {
            for tx in txs {
                all_txs.insert(tx.id(), tx);
            }
        }

        // Collect from pending_pledges
        for (_, inner_cache) in mempool_state_guard.pending_pledges.iter() {
            for (id, tx) in inner_cache.iter() {
                all_txs.insert(*id, tx);
            }
        }

        // Lookup requested transactions - O(n)
        commitment_tx_ids
            .iter()
            .filter_map(|tx_id| all_txs.get(tx_id).map(|tx| (*tx_id, (*tx).clone())))
            .collect()
    }

    /// Get specific data transactions by their IDs from the mempool
    ///
    /// This searches `valid_submit_ledger_tx` for the requested transactions.
    ///
    /// Returns a HashMap containing only the requested transactions that were found.
    ///
    /// Complexity: O(n) where n is the number of requested IDs.
    #[must_use]
    pub async fn get_data_txs(
        &self,
        data_tx_ids: &[IrysTransactionId],
    ) -> HashMap<IrysTransactionId, DataTransactionHeader> {
        let mempool_state_guard = self.read().await;
        let mut results = HashMap::with_capacity(data_tx_ids.len());
        for tx_id in data_tx_ids {
            if let Some(tx) = mempool_state_guard.valid_submit_ledger_tx.get(tx_id) {
                results.insert(*tx_id, tx.clone());
            }
        }
        results
    }

    // wipes all the "blacklists", primarily used after trying to restore the mempool from disk so that validation errors then (i.e if we have a saved tx that uses an anchor from some blocks that we forgot we when restarted) don't affect block validation
    // right now this only wipes `recent_invalid_tx`
    pub async fn wipe_blacklists(&self) {
        let mut write = self.write().await;
        write.recent_invalid_tx.clear();
        write.recent_invalid_payload_fingerprints.clear();
    }

    pub async fn get_status(
        &self,
        config: &NodeConfig,
        chunk_ingress: &ChunkIngressState,
    ) -> MempoolStatus {
        // Read chunk ingress count first to avoid holding the mempool lock across the await.
        let pending_chunks_count = chunk_ingress.pending_chunks_count().await;

        let state = self.read().await;

        // Calculate total data size
        let data_tx_total_size: u64 = state
            .valid_submit_ledger_tx
            .values()
            .map(|tx| tx.data_size)
            .sum();

        let mempool_config = &config.consensus_config().mempool;

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

        MempoolStatus {
            data_tx_count: state.valid_submit_ledger_tx.len(),
            commitment_tx_count: state.valid_commitment_tx.values().map(Vec::len).sum(),
            pending_chunks_count,
            pending_pledges_count: state.pending_pledges.len(),
            recent_valid_tx_count: state.recent_valid_tx.len(),
            recent_invalid_tx_count: state.recent_invalid_tx.len(),
            data_tx_total_size,
            config: mempool_config.clone(),
            data_tx_capacity_pct,
            commitment_address_capacity_pct,
        }
    }

    pub async fn mark_fingerprint_as_invalid(&self, fingerprint: H256) {
        self.0
            .write()
            .await
            .recent_invalid_payload_fingerprints
            .put(fingerprint, ());
    }

    pub async fn is_a_recent_invalid_fingerprint(&self, fingerprint: &H256) -> bool {
        self.0
            .read()
            .await
            .recent_invalid_payload_fingerprints
            .contains(fingerprint)
    }

    pub async fn extend_stake_and_pledge_whitelist(&self, new_entries: HashSet<IrysAddress>) {
        let mut state = self.write().await;
        state.stake_and_pledge_whitelist.extend(new_entries);
    }

    pub async fn get_stake_and_pledge_whitelist_cloned(&self) -> HashSet<IrysAddress> {
        let state = self.read().await;
        state.stake_and_pledge_whitelist.clone()
    }

    pub async fn valid_submit_ledger_tx_cloned(
        &self,
        tx_id: &H256,
    ) -> Option<DataTransactionHeader> {
        self.0
            .read()
            .await
            .valid_submit_ledger_tx
            .get(tx_id)
            .cloned()
    }

    pub async fn all_valid_submit_ledgers_cloned(&self) -> HashMap<H256, DataTransactionHeader> {
        self.read().await.valid_submit_ledger_tx.clone()
    }

    /// For confirmed blocks, we log warnings but don't fail if mempool is full
    pub async fn bounded_insert_data_tx(&self, header: DataTransactionHeader) {
        let mut mempool_guard = self.write().await;
        if let Err(e) = mempool_guard.bounded_insert_data_tx(header.clone()) {
            warn!(
                tx.id = ?header.id,
                error = ?e,
                "Failed to insert confirmed promoted tx into mempool (likely at capacity)"
            );
        }
        mempool_guard.recent_valid_tx.put(header.id, ());
    }

    pub async fn count_mempool_commitments(
        &self,
        user_address: &IrysAddress,
        commitment_type_filter: impl Fn(CommitmentTypeV2) -> bool,
        seen_ids: &mut HashSet<H256>,
    ) -> u64 {
        let mempool = self.read().await;

        mempool
            .valid_commitment_tx
            .get(user_address)
            .map(|txs| {
                txs.iter()
                    .filter(|tx| commitment_type_filter(tx.commitment_type()))
                    .filter(|tx| seen_ids.insert(tx.id()))
                    .count() as u64
            })
            .unwrap_or(0)
    }

    /// Inserts tx into the mempool and marks it as recently valid.
    /// Uses bounded insertion which may evict lowest-fee transactions when at capacity.
    pub async fn insert_tx_and_mark_valid(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        self.write().await.insert_tx_and_mark_valid(tx)
    }

    pub async fn all_valid_commitment_txs_cloned(
        &self,
    ) -> HashMap<IrysAddress, Vec<CommitmentTransaction>> {
        self.read().await.valid_commitment_tx.clone()
    }

    pub async fn all_valid_submit_ledger_ids(&self) -> Vec<H256> {
        let state = self.read().await;
        state
            .valid_submit_ledger_tx
            .keys()
            .copied()
            .collect::<Vec<_>>()
    }

    pub async fn all_valid_commitment_ledger_addresses(&self) -> Vec<IrysAddress> {
        let state = self.read().await;
        state
            .valid_commitment_tx
            .keys()
            .copied()
            .collect::<Vec<_>>()
    }

    pub async fn valid_commitment_txs_cloned(
        &self,
        address: &IrysAddress,
    ) -> Option<Vec<CommitmentTransaction>> {
        self.0
            .read()
            .await
            .valid_commitment_tx
            .get(address)
            .cloned()
    }

    pub async fn remove_valid_submit_ledger_tx(&self, tx_id: &H256) {
        let mut state = self.write().await;
        state.valid_submit_ledger_tx.remove(tx_id);
    }

    /// Set included_height for a data transaction with optional overwrite
    ///
    /// # Parameters
    /// - `tx_id`: Transaction ID to update
    /// - `height`: Block height to set
    /// - `overwrite`: If false, only sets height if currently None; if true, sets unconditionally
    ///
    /// Returns true if the included_height was actually changed, false otherwise.
    /// Also updates the recent_valid_tx cache when the transaction is found.
    async fn set_data_tx_included_height_inner(
        &self,
        tx_id: H256,
        height: u64,
        overwrite: bool,
    ) -> bool {
        let mut state = self.write().await;
        if let Some(wrapped_tx) = state.valid_submit_ledger_tx.get_mut(&tx_id) {
            let updated = overwrite || wrapped_tx.metadata().included_height.is_none();
            if updated {
                wrapped_tx.metadata_mut().included_height = Some(height);
                tracing::debug!(
                    tx.id = %tx_id,
                    included_height = height,
                    overwrite = overwrite,
                    "Set included_height in mempool"
                );
            }
            // Always update recent_valid_tx cache when tx is found
            state.recent_valid_tx.put(tx_id, ());
            updated
        } else {
            false
        }
    }

    /// Clear included_height for a data transaction (re-org handling)
    /// Returns true if the transaction was found and the height was cleared
    async fn clear_data_tx_included_height_inner(&self, tx_id: H256) -> bool {
        let mut state = self.write().await;
        if let Some(wrapped_tx) = state.valid_submit_ledger_tx.get_mut(&tx_id) {
            wrapped_tx.metadata_mut().included_height = None;
            true
        } else {
            false
        }
    }

    /// Set included_height for a commitment transaction
    /// Returns true if the transaction was found and updated
    pub async fn set_commitment_tx_included_height(&self, tx_id: H256, height: u64) -> bool {
        let mut state = self.write().await;

        // Check valid commitment transactions
        for txs in state.valid_commitment_tx.values_mut() {
            if let Some(tx) = txs.iter_mut().find(|t| t.id() == tx_id) {
                tx.metadata_mut().included_height = Some(height);
                return true;
            }
        }

        // Check pending pledges
        for (_, pledges_cache) in state.pending_pledges.iter_mut() {
            if let Some(tx) = pledges_cache.get_mut(&tx_id) {
                tx.metadata_mut().included_height = Some(height);
                return true;
            }
        }

        false
    }

    /// Clears the included_height for a commitment transaction.
    /// Returns true if the transaction was found.
    pub async fn clear_commitment_tx_included_height(&self, tx_id: H256) -> bool {
        let mut state = self.write().await;

        // Check valid commitment transactions
        for txs in state.valid_commitment_tx.values_mut() {
            if let Some(tx) = txs.iter_mut().find(|t| t.id() == tx_id) {
                tx.metadata_mut().included_height = None;
                return true;
            }
        }

        // Check pending pledges
        for (_, pledges_cache) in state.pending_pledges.iter_mut() {
            if let Some(tx) = pledges_cache.get_mut(&tx_id) {
                tx.metadata_mut().included_height = None;
                return true;
            }
        }

        false
    }

    /// Removes a commitment transaction with the specified transaction ID from the valid_commitment_tx map
    /// Returns true if the transaction was found and removed, false otherwise
    pub async fn remove_commitment_tx(&self, txid: &H256) -> bool {
        self.remove_commitment_txs([*txid]).await
    }

    /// Removes commitment transactions with the specified transaction IDs from the valid_commitment_tx map
    /// Returns true if any transactions were found and removed, false otherwise
    pub async fn remove_commitment_txs(&self, txids: impl IntoIterator<Item = H256>) -> bool {
        let mut found = false;
        let txids_set: HashSet<H256> = txids.into_iter().collect();
        let mut mempool_state_guard = self.write().await;

        // Remove all txids from recent_valid_tx cache
        for txid in &txids_set {
            mempool_state_guard.recent_valid_tx.pop(txid);
        }

        // Create a vector of addresses to update to avoid borrowing issues
        let addresses_to_check: Vec<IrysAddress> = mempool_state_guard
            .valid_commitment_tx
            .keys()
            .copied()
            .collect();

        for address in addresses_to_check {
            if let Some(transactions) = mempool_state_guard.valid_commitment_tx.get_mut(&address) {
                // Remove all transactions that match any of the txids
                let original_len = transactions.len();
                transactions.retain(|tx| !txids_set.contains(&tx.id()));

                if transactions.len() < original_len {
                    found = true;
                }

                // If the vector is now empty, remove the entry
                if transactions.is_empty() {
                    mempool_state_guard.valid_commitment_tx.remove(&address);
                }
            }
        }

        found
    }

    pub async fn take_all_valid_txs(
        &self,
    ) -> (
        HashMap<H256, DataTransactionHeader>,
        HashMap<IrysAddress, Vec<CommitmentTransaction>>,
    ) {
        let mut state = self.write().await;
        state.recent_valid_tx.clear();

        // Return the transactions directly (they already contain metadata)
        let data_txs = std::mem::take(&mut state.valid_submit_ledger_tx);
        let commitment_txs = std::mem::take(&mut state.valid_commitment_tx);

        (data_txs, commitment_txs)
    }

    /// Check if a transaction ID is in the recent valid transactions cache
    pub async fn is_recent_valid_tx(&self, tx_id: &H256) -> bool {
        self.read().await.recent_valid_tx.contains(tx_id)
    }

    /// Get transaction metadata from the mempool.
    ///
    /// Returns `Some(TxMetadata)` when a transaction with metadata is found in
    /// [`valid_submit_ledger_tx`], [`valid_commitment_tx`], or [`pending_pledges`].
    /// Returns `None` only when the transaction ID is not present in the mempool.
    ///
    /// [`get_tx_metadata`]: Self::get_tx_metadata
    /// [`TxMetadata`]: TxMetadata
    /// [`valid_submit_ledger_tx`]: MempoolState::valid_submit_ledger_tx
    /// [`valid_commitment_tx`]: MempoolState::valid_commitment_tx
    /// [`pending_pledges`]: MempoolState::pending_pledges
    pub async fn get_tx_metadata(&self, tx_id: &H256) -> Option<TxMetadata> {
        let state = self.read().await;

        // Check data transactions - metadata is embedded
        if let Some(wrapped_tx) = state.valid_submit_ledger_tx.get(tx_id) {
            return Some(TxMetadata::Data(*wrapped_tx.metadata()));
        }

        // Check commitment transactions - metadata is embedded
        for txs in state.valid_commitment_tx.values() {
            if let Some(tx) = txs.iter().find(|t| t.id() == *tx_id) {
                return Some(TxMetadata::Commitment(*tx.metadata()));
            }
        }

        // Check pending pledges - they also have metadata
        for (_, pledges_cache) in state.pending_pledges.iter() {
            if let Some(tx) = pledges_cache.peek(tx_id) {
                return Some(TxMetadata::Commitment(*tx.metadata()));
            }
        }

        None
    }

    pub async fn remove_transactions_from_pending_valid_pool(&self, submit_tx_ids: &[H256]) {
        let mut guard = self.write().await;
        for txid in submit_tx_ids {
            guard.valid_submit_ledger_tx.remove(txid);
            guard.recent_valid_tx.pop(txid);
        }
    }

    pub async fn update_submit_transaction(&self, tx: DataTransactionHeader) {
        self.0
            .write()
            .await
            .valid_submit_ledger_tx
            .entry(tx.id)
            .and_modify(|existing| {
                // Merge metadata: prefer incoming metadata fields when set, preserve existing otherwise
                let merged_metadata = existing.metadata().merge(tx.metadata());

                *existing = tx;
                existing.set_metadata(merged_metadata);
            });
    }

    pub async fn clear_promoted_height(&self, txid: H256) -> bool {
        let mut cleared = false;
        let mut state = self.write().await;
        if let Some(wrapped_header) = state.valid_submit_ledger_tx.get_mut(&txid) {
            // Clear promoted_height in metadata
            wrapped_header.metadata_mut().promoted_height = None;
            state.recent_valid_tx.put(txid, ());
            tracing::debug!(tx.id = %txid, "Cleared promoted_height in mempool");
            cleared = true;
        }
        cleared
    }

    /// Atomically sets the promoted_height on a transaction in the mempool.
    /// Returns the updated header if the tx was found, None otherwise.
    /// This method holds the write lock for the entire operation to prevent race conditions.
    pub async fn set_promoted_height(
        &self,
        txid: H256,
        height: u64,
    ) -> Option<DataTransactionHeader> {
        let mut state = self.write().await;
        let wrapped_header = state.valid_submit_ledger_tx.get_mut(&txid)?;
        if wrapped_header.promoted_height().is_none() {
            // Set promoted_height in metadata
            wrapped_header.metadata_mut().promoted_height = Some(height);
        }
        let result = wrapped_header.clone();
        tracing::debug!(tx.id = %txid, promoted_height = height, "Set promoted_height in mempool");
        state.recent_valid_tx.put(txid, ());
        Some(result)
    }

    /// Atomically sets the included_height on a data transaction in the mempool.
    /// This is a convenience wrapper around set_tx_included_height with overwrite=false.
    /// Returns true if the tx was found and updated, false otherwise.
    pub async fn set_data_tx_included_height(&self, txid: H256, height: u64) -> bool {
        // Use the consolidated method with overwrite=false to maintain backward compatibility
        self.set_data_tx_included_height_inner(txid, height, false)
            .await
    }

    /// Set included_height for a data transaction with overwrite enabled
    /// This is used when processing canonical confirmations to ensure the height
    /// is updated even if previously set (e.g., after a reorg)
    pub async fn set_data_tx_included_height_overwrite(&self, txid: H256, height: u64) -> bool {
        self.set_data_tx_included_height_inner(txid, height, true)
            .await
    }

    /// Applies all metadata updates for a confirmed block under a single write lock.
    ///
    /// This batches the work of `set_data_tx_included_height_overwrite`,
    /// `set_commitment_tx_included_height`, and `set_promoted_height` to avoid
    /// acquiring the write lock once per transaction (~120 times per block).
    pub async fn apply_block_confirmed_updates(
        &self,
        submit_and_publish_txids: &[H256],
        commitment_txids: &[H256],
        publish_txids: &[H256],
        height: u64,
    ) {
        let mut state = self.write().await;

        // 1. Set included_height (with overwrite) for submit + publish data txs
        for txid in submit_and_publish_txids {
            if let Some(wrapped_tx) = state.valid_submit_ledger_tx.get_mut(txid) {
                wrapped_tx.metadata_mut().included_height = Some(height);
                tracing::debug!(
                    tx.id = %txid,
                    included_height = height,
                    overwrite = true,
                    "Set included_height in mempool"
                );
                state.recent_valid_tx.put(*txid, ());
            }
        }

        // 2. Set included_height for commitment txs
        'next_commitment: for tx_id in commitment_txids {
            // Check valid commitment transactions
            for txs in state.valid_commitment_tx.values_mut() {
                if let Some(tx) = txs.iter_mut().find(|t| t.id() == *tx_id) {
                    tx.metadata_mut().included_height = Some(height);
                    continue 'next_commitment;
                }
            }

            // Check pending pledges
            for (_, pledges_cache) in state.pending_pledges.iter_mut() {
                if let Some(tx) = pledges_cache.get_mut(tx_id) {
                    tx.metadata_mut().included_height = Some(height);
                    continue 'next_commitment;
                }
            }
        }

        // 3. Set promoted_height for publish txs
        for txid in publish_txids {
            if let Some(wrapped_header) = state.valid_submit_ledger_tx.get_mut(txid) {
                if wrapped_header.metadata().promoted_height.is_none() {
                    wrapped_header.metadata_mut().promoted_height = Some(height);
                }
                tracing::debug!(tx.id = %txid, promoted_height = height, "Set promoted_height in mempool");
                state.recent_valid_tx.put(*txid, ());
            }
        }
    }

    /// Atomically clears the included_height on a data transaction in the mempool.
    /// Returns true if the tx was found and updated, false otherwise.
    pub async fn clear_data_tx_included_height(&self, txid: H256) -> bool {
        self.clear_data_tx_included_height_inner(txid).await
    }

    pub async fn put_recent_invalid(&self, tx_id: H256) {
        self.write().await.put_recent_invalid(tx_id);
    }

    pub async fn mempool_data_tx_status(&self, txid: &H256) -> Option<TxKnownStatus> {
        let mempool_state_guard = self.read().await;

        // #[expect(clippy::if_same_then_else, reason = "readability")]
        if mempool_state_guard
            .valid_submit_ledger_tx
            .contains_key(txid)
        {
            Some(TxKnownStatus::Valid)
        } else if mempool_state_guard.recent_valid_tx.contains(txid) {
            Some(TxKnownStatus::ValidSeen)
        } else if mempool_state_guard.recent_invalid_tx.contains(txid) {
            // Still has it, just invalid
            Some(TxKnownStatus::InvalidSeen)
        } else {
            None
        }
    }

    pub async fn mempool_commitment_tx_status(
        &self,
        commitment_tx_id: &H256,
    ) -> Option<TxKnownStatus> {
        let mempool_state_guard = self.read().await;

        #[expect(clippy::if_same_then_else, reason = "readability")]
        if mempool_state_guard
            .recent_valid_tx
            .contains(commitment_tx_id)
        {
            Some(TxKnownStatus::ValidSeen)
        } else if mempool_state_guard
            .recent_invalid_tx
            .contains(commitment_tx_id)
        {
            // Still has it, just invalid
            Some(TxKnownStatus::InvalidSeen)
            // Get any CommitmentTransactions from the valid commitments Map
        } else if mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .any(|tx| &tx.id() == commitment_tx_id)
        {
            Some(TxKnownStatus::Valid)
        }
        // Get any CommitmentTransactions from the pending commitments LRU cache
        else if mempool_state_guard
            .pending_pledges
            .iter()
            .flat_map(|(_, inner)| inner.iter())
            .any(|(id, _tx)| id == commitment_tx_id)
        {
            Some(TxKnownStatus::Valid)
        } else {
            None
        }
    }

    pub async fn is_address_in_a_whitelist(&self, address: &IrysAddress) -> bool {
        let read_guard = self.read().await;
        let whitelist = &read_guard.stake_and_pledge_whitelist;
        whitelist.is_empty() || whitelist.contains(address)
    }

    /// Returns true if the commitment tx is already known in the mempool caches/maps.
    pub async fn is_known_commitment_in_mempool(&self, tx_id: &H256, signer: IrysAddress) -> bool {
        let guard = self.read().await;
        // Only treat recent valid entries as known. Invalid must not block legitimate re-ingress.
        if guard.recent_valid_tx.contains(tx_id) {
            return true;
        }
        if guard
            .valid_commitment_tx
            .get(&signer)
            .is_some_and(|txs| txs.iter().any(|c| c.id() == *tx_id))
        {
            return true;
        }
        false
    }

    /// should really only be called by persist_mempool_to_disk, all other scenarios need a more
    /// subtle filtering of commitment state, recently confirmed? pending? valid? etc.
    pub async fn get_all_commitment_tx(&self) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state_guard = self.read().await;

        // Get any CommitmentTransactions from the valid commitments
        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .for_each(|tx| {
                hash_map.insert(tx.id(), tx.clone());
            });

        // Get any CommitmentTransactions from the pending commitments
        mempool_state_guard
            .pending_pledges
            .iter()
            .flat_map(|(_, inner)| inner.iter())
            .for_each(|(tx_id, tx)| {
                hash_map.insert(*tx_id, tx.clone());
            });

        hash_map
    }

    pub async fn pop_pending_pledges_for_signer(
        &self,
        signer: &IrysAddress,
    ) -> Option<LruCache<IrysTransactionId, CommitmentTransaction>> {
        self.write().await.pending_pledges.pop(signer)
    }

    /// Caches an unstaked pledge in the two-level LRU structure.
    pub async fn cache_unstaked_pledge(
        &self,
        tx: &CommitmentTransaction,
        max_pending_pledge_items: usize,
    ) {
        self.write()
            .await
            .cache_unstaked_pledge(tx, max_pending_pledge_items);
    }

    /// Inserts a commitment into the mempool valid map and marks it as recently valid.
    /// Uses bounded insertion which may evict transactions when limits are exceeded.
    async fn insert_commitment_and_mark_valid(
        &self,
        tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        self.write().await.insert_commitment_and_mark_valid(tx)
    }

    async fn is_there_a_pledge_for_unstaked_address(&self, signer: &IrysAddress) -> bool {
        // For unstaked addresses, check for pending stake transactions
        let mempool_state_guard = self.read().await;
        // Get pending transactions for this address
        if let Some(pending) = mempool_state_guard.valid_commitment_tx.get(signer) {
            // Check if there's at least one pending stake transaction
            if pending
                .iter()
                .any(|c| c.commitment_type() == CommitmentTypeV2::Stake)
            {
                return true;
            }
        }

        false
    }

    // --- Batch methods for reduced lock contention ---

    /// Batch lookup data transactions from mempool in a single READ lock.
    pub async fn batch_valid_submit_ledger_tx_cloned(
        &self,
        tx_ids: &[H256],
    ) -> Vec<Option<DataTransactionHeader>> {
        let state = self.read().await;
        tx_ids
            .iter()
            .map(|tx_id| state.valid_submit_ledger_tx.get(tx_id).cloned())
            .collect()
    }

    /// Returns a write guard for callers that need to hold the lock across
    /// multiple MempoolState method calls (e.g., reorg revalidation).
    pub(crate) async fn write_for_reorg(&self) -> tokio::sync::RwLockWriteGuard<'_, MempoolState> {
        self.write().await
    }

    /// Do not call this function from anywhere outside AtomicMempoolState
    #[instrument(skip_all)]
    async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, MempoolState> {
        tokio::time::timeout(Duration::from_secs(10), self.0.read()).await.unwrap_or_else(|elapsed| {
            error!("Timed out waiting for mempool read lock after 10s: {elapsed}, possibly due to a deadlock");
            panic!("Timed out waiting for mempool read lock after 10s: {elapsed}, possibly due to a deadlock")
        })
    }

    /// Do not call this function from anywhere outside AtomicMempoolState
    #[instrument(skip_all)]
    async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, MempoolState> {
        tokio::time::timeout(Duration::from_secs(10), self.0.write()).await.unwrap_or_else(|elapsed| {
            error!("Timed out waiting for mempool write lock after: {elapsed}, possibly due to a deadlock");
            panic!("Timed out waiting for mempool write lock after: {elapsed}, possibly due to a deadlock")
        })
    }
}

#[derive(Debug)]
pub struct MempoolState {
    /// bounded map with manual capacity enforcement
    pub valid_submit_ledger_tx: HashMap<H256, DataTransactionHeader>,
    pub max_submit_txs: usize,
    /// bounded map with manual capacity enforcement
    pub valid_commitment_tx: HashMap<IrysAddress, Vec<CommitmentTransaction>>,
    pub max_commitment_addresses: usize,
    pub max_commitments_per_address: usize,
    /// The miner's signer instance, used to sign ingress proofs
    pub recent_invalid_tx: LruCache<H256, ()>,
    /// Tracks recent invalid payload fingerprints (e.g., keccak(prehash + signature)) for signature-invalid payloads
    /// Prevents poisoning legitimate txids via mismatched id/signature pairs
    pub recent_invalid_payload_fingerprints: LruCache<H256, ()>,
    /// Tracks recent valid txids from either data or commitment
    pub recent_valid_tx: LruCache<H256, ()>,
    pub pending_pledges: LruCache<IrysAddress, LruCache<IrysTransactionId, CommitmentTransaction>>,
    pub stake_and_pledge_whitelist: HashSet<IrysAddress>,
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(
    config: &MempoolConfig,
    stake_and_pledge_whitelist: &[IrysAddress],
) -> MempoolState {
    let max_pending_pledge_items = config.max_pending_pledge_items;
    MempoolState {
        valid_submit_ledger_tx: HashMap::new(),
        max_submit_txs: config.max_valid_submit_txs,
        valid_commitment_tx: HashMap::new(),
        max_commitment_addresses: config.max_valid_commitment_addresses,
        max_commitments_per_address: config.max_commitments_per_address,
        recent_invalid_tx: LruCache::new(NonZeroUsize::new(config.max_invalid_items).unwrap()),
        recent_invalid_payload_fingerprints: LruCache::new(
            NonZeroUsize::new(config.max_invalid_items).unwrap(),
        ),
        recent_valid_tx: LruCache::new(NonZeroUsize::new(config.max_valid_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
        stake_and_pledge_whitelist: HashSet::from_iter(stake_and_pledge_whitelist.iter().copied()),
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
        use std::collections::hash_map::Entry;

        // If tx already exists we still update it.
        // the new entry might have the `is_promoted` flag set on it, which is needed for correct promotion logic
        // Preserve metadata when updating
        if let Entry::Occupied(mut entry) = self.valid_submit_ledger_tx.entry(tx.id) {
            // Merge metadata: prefer incoming metadata fields when set, preserve existing otherwise
            let merged_metadata = entry.get().metadata().merge(tx.metadata());
            let mut new_tx = tx;
            new_tx.set_metadata(merged_metadata);
            entry.insert(new_tx);
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
    /// Returns (tx_id, fee) tuple. Ties are broken by tx id for determinism.
    fn find_lowest_fee_data_tx(&self) -> Option<(H256, BoundedFee)> {
        self.valid_submit_ledger_tx
            .iter()
            .min_by(|(id_a, tx_a), (id_b, tx_b)| {
                tx_a.user_fee()
                    .cmp(&tx_b.user_fee())
                    .then_with(|| id_a.cmp(id_b))
            })
            .map(|(id, wrapped_tx)| (*id, wrapped_tx.user_fee()))
    }

    /// Insert commitment tx with bounds enforcement.
    /// Enforces per-address limit and global address limit.
    /// Rejects new commitments when per-address limit is exceeded (preserves existing commitments).
    /// Evicts lowest-value addresses when global address limit is exceeded.
    pub fn bounded_insert_commitment_tx(
        &mut self,
        tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let address = tx.signer();
        let tx_id = tx.id();

        // Check for duplicate tx.id - if already exists, just return Ok()
        if let Some(existing_txs) = self.valid_commitment_tx.get(&address)
            && existing_txs.iter().any(|t| t.id() == tx_id)
        {
            return Ok(()); // Duplicate, already have this commitment
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
                        new_value, evict_total_value
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
                new.tx_id = ?tx_id,
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
    /// Returns (Address, total_value) tuple. Ties are broken by address for determinism.
    fn find_lowest_value_address(&self) -> Option<(IrysAddress, U256)> {
        self.valid_commitment_tx
            .iter()
            .map(|(addr, txs)| {
                let total = txs
                    .iter()
                    .map(irys_types::CommitmentTransaction::total_cost)
                    .fold(U256::zero(), irys_types::U256::saturating_add);
                (*addr, total)
            })
            .min_by(|(addr_a, total_a), (addr_b, total_b)| {
                total_a.cmp(total_b).then_with(|| addr_a.cmp(addr_b))
            })
    }

    /// Marks a given tx as invalid, adding its ID to `recent_invalid_tx` and removing it from `recent_valid_tx`.
    pub fn mark_tx_as_invalid(&mut self, tx_id: IrysTransactionId, err_reason: impl ToString) {
        warn!("Tx {} is invalid: {:?}", &tx_id, &err_reason.to_string());
        self.recent_invalid_tx.put(tx_id, ());
        self.recent_valid_tx.pop(&tx_id);
    }

    /// Batch-removes expired data txs.
    /// Each entry is (tx_id, anchor) — the anchor is used for the InvalidAnchor error reason.
    pub fn batch_prune_data_txs(&mut self, expired: &[(H256, H256)]) {
        for &(tx_id, anchor) in expired {
            self.valid_submit_ledger_tx.remove(&tx_id);
            warn!(
                "Tx {} is invalid: {:?}",
                &tx_id,
                &TxIngressError::InvalidAnchor(anchor).to_string()
            );
            self.recent_invalid_tx.put(tx_id, ());
            self.recent_valid_tx.pop(&tx_id);
        }
    }

    /// Batch-removes expired commitment txs.
    /// Each entry is (tx_id, anchor) — the anchor is used for the InvalidAnchor error reason.
    pub fn batch_prune_commitment_txs(&mut self, expired: &[(H256, H256)]) {
        let txids_set: HashSet<H256> = expired.iter().map(|&(tx_id, _)| tx_id).collect();

        // Mark all as invalid and remove from recent_valid_tx
        for &(tx_id, anchor) in expired {
            warn!(
                "Tx {} is invalid: {:?}",
                &tx_id,
                &TxIngressError::InvalidAnchor(anchor).to_string()
            );
            self.recent_invalid_tx.put(tx_id, ());
            self.recent_valid_tx.pop(&tx_id);
        }

        // Remove from valid_commitment_tx map
        let addresses_to_check: Vec<IrysAddress> =
            self.valid_commitment_tx.keys().copied().collect();
        for address in addresses_to_check {
            if let Some(transactions) = self.valid_commitment_tx.get_mut(&address) {
                transactions.retain(|tx| !txids_set.contains(&tx.id()));
                if transactions.is_empty() {
                    self.valid_commitment_tx.remove(&address);
                }
            }
        }
    }

    /// Inserts tx into the mempool and marks it as recently valid.
    /// Uses bounded insertion which may evict lowest-fee transactions when at capacity.
    pub fn insert_tx_and_mark_valid(
        &mut self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        self.bounded_insert_data_tx(tx.clone())?;
        self.recent_valid_tx.put(tx.id, ());
        Ok(())
    }

    /// Adds a tx ID to the recent invalid set.
    pub fn put_recent_invalid(&mut self, tx_id: H256) {
        self.recent_invalid_tx.put(tx_id, ());
    }

    /// Caches an unstaked pledge in the two-level LRU structure.
    pub fn cache_unstaked_pledge(
        &mut self,
        tx: &CommitmentTransaction,
        max_pending_pledge_items: usize,
    ) {
        if let Some(pledges_cache) = self.pending_pledges.get_mut(&tx.signer()) {
            // Address already exists in cache - add this pledge transaction to its lru cache
            pledges_cache.put(tx.id(), tx.clone());
        } else {
            // First pledge from this address - create a new nested lru cache
            let mut new_address_cache =
                LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap());

            // Add the pledge transaction to the new lru cache for the address
            new_address_cache.put(tx.id(), tx.clone());

            // Add the address cache to the primary lru cache
            self.pending_pledges.put(tx.signer(), new_address_cache);
        }
    }

    /// Inserts a commitment into the mempool valid map and marks it as recently valid.
    /// Uses bounded insertion which may evict transactions when limits are exceeded.
    pub fn insert_commitment_and_mark_valid(
        &mut self,
        tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        self.bounded_insert_commitment_tx(tx)?;
        self.recent_valid_tx.put(tx.id(), ());
        Ok(())
    }

    /// Revalidates data txs in-place. Removes those with expired/invalid anchors.
    /// Uses batch_prune_data_txs for the actual removal (same method used by prune_pending_txs).
    pub fn revalidate_data_txs(
        &mut self,
        mut should_prune: impl FnMut(&DataTransactionHeader) -> bool,
    ) {
        // Collect txs to prune
        let expired: Vec<(H256, H256)> = self
            .valid_submit_ledger_tx
            .values()
            .filter(|tx| should_prune(tx))
            .map(|tx| (tx.id, tx.anchor))
            .collect();

        if !expired.is_empty() {
            // Reuse the same prune method that prune_pending_txs uses
            self.batch_prune_data_txs(&expired);
        }
    }

    /// Revalidates commitment txs in-place. Removes those with expired anchors.
    /// Moves Unstaked commitments (with no pending Stake in the valid set) to pending_pledges
    /// using the same cache_unstaked_pledge method that ingress uses.
    ///
    /// Note: `pending_pledges` is intentionally not revalidated here. Those txs are
    /// already parked waiting for a future Stake; anchor expiry will be checked when
    /// they are re-ingested via `process_pending_pledges_for_new_stake`.
    pub fn revalidate_commitment_txs(
        &mut self,
        mut should_prune: impl FnMut(&CommitmentTransaction) -> bool,
        mut get_snapshot_status: impl FnMut(&CommitmentTransaction) -> CommitmentSnapshotStatus,
        max_pending_pledge_items: usize,
    ) {
        // Pass 1: Identify pruned txs and addresses with valid Stake txs
        let mut pruned: Vec<(H256, H256)> = Vec::new();
        let mut addresses_with_valid_stake: HashSet<IrysAddress> = HashSet::new();

        for (_addr, txs) in self.valid_commitment_tx.iter() {
            for tx in txs {
                if should_prune(tx) {
                    pruned.push((tx.id(), tx.anchor()));
                } else if tx.commitment_type() == CommitmentTypeV2::Stake {
                    addresses_with_valid_stake.insert(tx.signer());
                }
            }
        }

        // Pass 2: Identify txs to move to pending (Unstaked without pending Stake)
        let pruned_set: HashSet<H256> = pruned.iter().map(|(id, _)| *id).collect();
        let mut to_pending: Vec<CommitmentTransaction> = Vec::new();
        for (_addr, txs) in self.valid_commitment_tx.iter() {
            for tx in txs {
                if pruned_set.contains(&tx.id()) {
                    continue;
                }
                if get_snapshot_status(tx) == CommitmentSnapshotStatus::Unstaked
                    && !addresses_with_valid_stake.contains(&tx.signer())
                {
                    to_pending.push(tx.clone());
                }
            }
        }

        // Phase 3: Apply mutations using shared methods
        if !pruned.is_empty() {
            self.batch_prune_commitment_txs(&pruned); // same method as prune_pending_txs
        }

        // Remove moved-to-pending txs from valid set
        let moved_set: HashSet<H256> = to_pending.iter().map(CommitmentTransaction::id).collect();
        if !moved_set.is_empty() {
            self.valid_commitment_tx.retain(|_addr, txs| {
                txs.retain(|tx| !moved_set.contains(&tx.id()));
                !txs.is_empty()
            });

            // Clear recent_valid_tx for moved txs so re-ingress via
            // process_pending_pledges_for_new_stake is not blocked by
            // is_known_commitment_in_mempool seeing a stale entry.
            for tx_id in &moved_set {
                self.recent_valid_tx.pop(tx_id);
            }
        }

        // Cache as unstaked using same method as ingress Unstaked path
        for tx in &to_pending {
            self.cache_unstaked_pledge(tx, max_pending_pledge_items);
        }
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
    #[error("Transaction signature is invalid for address {0}")]
    InvalidSignature(IrysAddress),
    /// The commitment transaction version is below minimum required after hardfork activation
    #[error("Commitment transaction version {version} is below minimum required version {minimum}")]
    InvalidVersion { version: u8, minimum: u8 },
    /// UpdateRewardAddress commitment type is not allowed before Borealis hardfork activation
    #[error("UpdateRewardAddress commitment type not allowed before Borealis hardfork")]
    UpdateRewardAddressNotAllowed,
    /// The account does not have enough tokens to fund this transaction
    #[error("Account has insufficient funds for transaction {0}")]
    Unfunded(H256),
    /// This transaction id is already in the cache
    #[error("Transaction already exists in cache")]
    Skipped,
    /// Invalid anchor value (unknown or too old)
    #[error("Anchor {0} is either unknown or has expired")]
    InvalidAnchor(H256),
    /// Invalid ledger type specified in transaction
    #[error("Invalid or unsupported ledger ID: {0}")]
    InvalidLedger(u32),
    /// Some database error occurred
    #[error("Database operation failed: {0}")]
    DatabaseError(String),
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

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<MempoolServiceMessage>>, // mempool message receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,                // reorg broadcast receiver
    inner: Arc<Inner>,
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
        block_tree_read_guard: &BlockTreeReadGuard,
        rx: UnboundedReceiver<Traced<MempoolServiceMessage>>,
        config: &Config,
        service_senders: &ServiceSenders,
        runtime_handle: tokio::runtime::Handle,
        chunk_ingress_state: ChunkIngressState,
    ) -> eyre::Result<TokioServiceHandle> {
        info!("Spawning mempool service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let initial_stake_and_pledge_whitelist = config
            .node_config
            .initial_stake_and_pledge_whitelist
            .clone();
        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.mempool;
        let max_concurrent_mempool_tasks = mempool_config.max_concurrent_mempool_tasks;
        let mempool_state = create_state(mempool_config, &initial_stake_and_pledge_whitelist);
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();

        let handle_for_inner = runtime_handle.clone();
        let handle = runtime_handle.spawn(
            async move {
                let mempool_state = AtomicMempoolState::new(mempool_state);
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
                    inner: Arc::new(Inner {
                        block_tree_read_guard,
                        config,
                        exec: TaskExecutor::current(),
                        irys_db,
                        mempool_state,
                        reth_node_adapter,
                        service_senders,
                        pledge_provider,
                        message_handler_semaphore: Arc::new(Semaphore::new(
                            max_concurrent_mempool_tasks,
                        )),
                        chunk_ingress_state,
                    }),
                };
                mempool_service
                    .start(handle_for_inner)
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

    async fn start(mut self, runtime_handle: tokio::runtime::Handle) -> eyre::Result<()> {
        tracing::info!("starting Mempool service");

        self.inner.restore_mempool_from_disk().await;
        self.inner.reconstruct_metadata_from_db().await;

        let mut shutdown_future = pin!(self.shutdown);
        loop {
            tokio::select! {
                // Handle regular mempool messages
                traced = self.msg_rx.recv() => {
                    match traced {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            let msg_type = msg.variant_name();
                            let span = tracing::info_span!(parent: &parent_span, "mempool_handle_message", msg_type = %msg_type);

                            let semaphore = self.inner.message_handler_semaphore.clone();
                            match semaphore.try_acquire_owned() {
                                Ok(permit) => {
                                    let inner = Arc::clone(&self.inner);
                                    runtime_handle.spawn(async move {
                                        let _permit = permit;
                                        let task_info = format!("Mempool message handler for {}", msg_type);
                                        if let Err(err) = wait_with_progress(
                                            inner.handle_message(msg),
                                            20,
                                            &task_info,
                                        ).await {
                                            error!("Error handling mempool message {}: {:?}", msg_type, err);
                                        }
                                    }.instrument(span));
                                }
                                Err(e) => {
                                    match e {
                                        tokio::sync::TryAcquireError::Closed => {
                                            error!("Mempool message handler semaphore closed");
                                            break;
                                        }
                                        tokio::sync::TryAcquireError::NoPermits => {
                                            warn!("Mempool message handler semaphore at capacity, waiting for permit");
                                        }
                                    }
                                    let inner = Arc::clone(&self.inner);
                                    let semaphore = inner.message_handler_semaphore.clone();
                                    match tokio::time::timeout(Duration::from_secs(60), semaphore.acquire_owned()).await {
                                        Ok(permit_result) => {
                                            match permit_result {
                                                Ok(permit) => {
                                                    runtime_handle.spawn(async move {
                                                        let _permit = permit;
                                                        let task_info = format!("Mempool message handler for {}", msg_type);
                                                        if let Err(err) = wait_with_progress(
                                                            inner.handle_message(msg),
                                                            20,
                                                            &task_info,
                                                        ).await {
                                                            error!("Error handling mempool message {}: {:?}", msg_type, err);
                                                        }
                                                    }.instrument(span));
                                                }
                                                Err(err) => {
                                                    error!("Failed to acquire mempool message handler permit: {:?}", err);
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            error!("Timed out waiting for mempool message handler permit");
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            warn!("receiver channel closed");
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

                // Handle shutdown signal
                _ = &mut shutdown_future => {
                    info!("Shutdown signal received for mempool service");
                    break;
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");

        // Process remaining messages with timeout
        let process_remaining = async {
            while let Ok(traced) = self.msg_rx.try_recv() {
                let (msg, parent_span) = traced.into_parts();
                let span = tracing::info_span!(parent: &parent_span, "mempool_handle_message", msg_type = %msg.variant_name());
                self.inner.handle_message(msg).instrument(span).await?;
            }
            Ok::<(), eyre::Error>(())
        };

        match tokio::time::timeout(Duration::from_secs(10), process_remaining).await {
            Ok(Ok(())) => tracing::debug!("Processed remaining messages successfully"),
            Ok(Err(e)) => tracing::error!("Error processing remaining messages: {:?}", e),
            Err(_) => tracing::warn!("Timeout processing remaining messages, continuing shutdown"),
        }

        // Persist to disk with timeout
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.inner.persist_mempool_to_disk(),
        )
        .await
        {
            Ok(Ok(())) => tracing::debug!("Persisted mempool to disk successfully"),
            Ok(Err(e)) => tracing::error!("Error persisting mempool to disk: {:?}", e),
            Err(_) => tracing::warn!("Timeout persisting mempool to disk, continuing shutdown"),
        }

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

/// Waits for `fut` to finish while printing every `n_secs`.
pub(crate) async fn wait_with_progress<F, T>(fut: F, n_secs: u64, task_info: &str) -> T
where
    F: std::future::Future<Output = T>,
{
    let span = Span::current();
    let fut = fut.instrument(Span::current());
    tokio::pin!(fut);

    let mut ticker = tokio::time::interval(Duration::from_secs(n_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Don't do an immediate tick
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let _guard = span.enter();
                warn!("Task {task_info} takes too long to complete, possible deadlock detected...");
            }
            res = &mut fut => {
                break res;
            }
        }
    }
}

#[cfg(test)]
mod bounded_mempool_tests {
    use super::*;
    use irys_types::{
        CommitmentTransactionV2, CommitmentTypeV2, DataLedger, DataTransactionHeaderV1,
        DataTransactionMetadata, IrysSignature,
    };

    // ========================================================================
    // Test Helpers
    // ========================================================================

    /// Creates a test data transaction with specified fee
    fn create_test_data_tx(fee: u64) -> DataTransactionHeader {
        DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                anchor: H256::zero(),
                signer: IrysAddress::random(),
                data_root: H256::random(),
                data_size: 1024,
                header_size: 0,
                term_fee: U256::from(fee).into(),
                perm_fee: Some(U256::from(100).into()),
                ledger_id: DataLedger::Publish as u32,
                bundle_format: Some(0),
                signature: IrysSignature::default(),
                chain_id: 1,
            },
            metadata: DataTransactionMetadata::new(),
        })
    }

    /// Creates a test commitment transaction with specified signer and value
    fn create_test_commitment_tx(signer: IrysAddress, value: u64) -> CommitmentTransaction {
        CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2 {
                id: H256::random(), // Random ID for testing
                anchor: H256::zero(),
                signer,
                signature: IrysSignature::default(),
                fee: 100,
                value: U256::from(value),
                commitment_type: CommitmentTypeV2::Stake,
                chain_id: 1,
            },
            metadata: Default::default(),
        })
    }

    /// Creates a test mempool state with specified capacities
    fn create_test_mempool_state(
        max_data: usize,
        max_addresses: usize,
        max_per_address: usize,
    ) -> MempoolState {
        MempoolState {
            valid_submit_ledger_tx: HashMap::new(),
            max_submit_txs: max_data,
            valid_commitment_tx: HashMap::new(),
            max_commitment_addresses: max_addresses,
            max_commitments_per_address: max_per_address,
            recent_invalid_tx: LruCache::new(NonZeroUsize::new(100).unwrap()),
            recent_invalid_payload_fingerprints: LruCache::new(NonZeroUsize::new(100).unwrap()),
            recent_valid_tx: LruCache::new(NonZeroUsize::new(100).unwrap()),
            pending_pledges: LruCache::new(NonZeroUsize::new(10).unwrap()),
            stake_and_pledge_whitelist: HashSet::new(),
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

        let address = IrysAddress::random();
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
        assert!(txs.iter().any(|t| t.id() == tx1.id()));
        assert!(txs.iter().any(|t| t.id() == tx2.id()));
        assert!(txs.iter().any(|t| t.id() == tx3.id()));
        assert!(!txs.iter().any(|t| t.id() == tx4.id()));
    }

    #[test]
    fn test_commitment_tx_different_addresses_independent_limits() {
        // Setup: Max 2 commitments per address
        let mut state = create_test_mempool_state(10, 10, 2);

        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();

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

        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();
        let addr_c = IrysAddress::random();
        let addr_d = IrysAddress::random();

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

        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();
        let addr_c = IrysAddress::random();

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
