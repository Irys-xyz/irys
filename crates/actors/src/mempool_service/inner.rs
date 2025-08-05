use crate::block_discovery::get_data_tx_in_parallel_inner;
use crate::mempool_service::{ChunkIngressError, MempoolPledgeProvider};
use crate::services::ServiceSenders;
use base58::ToBase58 as _;
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
use irys_types::{
    app_state::DatabaseProvider, Config, IrysBlockHeader, IrysTransactionCommon, IrysTransactionId,
    H256, U256,
};
use irys_types::{
    Address, Base64, CommitmentTransaction, CommitmentValidationError, DataRoot,
    DataTransactionHeader, MempoolConfig, TxChunkOffset, TxIngressProof, UnpackedChunk,
};
use lru::LruCache;
use reth::rpc::types::BlockId;
use reth::tasks::TaskExecutor;
use reth_db::cursor::*;
use reth_db::{Database as _, DatabaseError};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::fs;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, error, info, instrument, trace, warn};

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
            }
            Ok(())
        })
    }

    #[instrument(skip(self), fields(parent_block_id = ?parent_evm_block_id), err)]
    async fn handle_get_best_mempool_txs(
        &self,
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

        // Get a list of all recently confirmed commitment txids in the canonical chain
        let (canonical, _) = self.block_tree_read_guard.read().get_canonical_chain();

        let last_block = canonical.last().ok_or_eyre("Empty canonical chain")?;

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
        let mut simulation_commitment_snapshot = self
            .block_tree_read_guard
            .read()
            .canonical_commitment_snapshot()
            .as_ref()
            .clone();
        let epoch_snapshot = self.block_tree_read_guard.read().canonical_epoch_snapshot();
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

            // signer stake status check
            if matches!(tx.commitment_type, CommitmentType::Stake) {
                let epoch_snapshot = self
                    .block_tree_read_guard
                    .read()
                    .get_epoch_snapshot(&last_block.block_hash)
                    .expect("parent blocks epoch_snapshot should be retrievable");
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

        // Sort data transactions by fee (highest first) to maximize revenue
        let total_data_available = submit_ledger_txs.len();

        submit_ledger_txs.sort_by(|a, b| match b.user_fee().cmp(&a.user_fee()) {
            std::cmp::Ordering::Equal => a.id.cmp(&b.id),
            fee_ordering => fee_ordering,
        });

        // Apply block size constraint and funding checks to data transactions
        let mut submit_tx = Vec::new();
        let max_data_txs = self
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
            publish_txs = publish_txs_and_proofs.0.len(),
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

    pub async fn get_publish_txs_and_proofs(
        &self,
    ) -> Result<(Vec<DataTransactionHeader>, Vec<TxIngressProof>), eyre::Error> {
        let mut publish_txs: Vec<DataTransactionHeader> = Vec::new();
        let mut publish_proofs: Vec<TxIngressProof> = Vec::new();

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
                let has_ingress_proof = tx_header.ingress_proofs.is_some();
                debug!(
                    "Publish candidate {} has ingress proof? {}",
                    &tx_header.id, &has_ingress_proof
                );
                // If there's no ingress proof included in the tx header, it means the tx still needs to be promoted
                if !has_ingress_proof {
                    // Get the proofs for this tx
                    let proofs = ingress_proofs_by_data_root(&read_tx, tx_header.data_root)?;
                    // TODO: replace this section to properly handle multiple ingress proofs
                    match proofs.first() {
                        Some((_data_root, proof)) => {
                            let mut tx_header = tx_header.clone();
                            let proof = TxIngressProof {
                                proof: proof.proof.proof,
                                signature: proof.proof.signature,
                            };
                            debug!(
                                "Got ingress proof {} for publish candidate {}",
                                &tx_header.data_root, &tx_header.id
                            );
                            publish_proofs.push(proof.clone());
                            tx_header.ingress_proofs = Some(proof);
                            publish_txs.push(tx_header)
                        }
                        None => {
                            error!(
                                "No ingress proof found for data_root: {} tx: {}",
                                tx_header.data_root, &tx_header.id
                            );
                            continue;
                        }
                    }
                }
            }
        }

        let txs = &publish_txs
            .iter()
            .map(|h| h.id.0.to_base58())
            .collect::<Vec<_>>();
        debug!(?txs, "Publish transactions");
        Ok((publish_txs, publish_proofs))
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

    // Helper to validate anchor
    // this takes in an IrysTransaction and validates the anchor
    // if the anchor is valid, returns the tx back with the height that made the anchor canonical (i.e the block height)
    #[instrument(skip_all, fields(tx_id = %tx.id(), anchor = %tx.anchor()))]
    pub async fn validate_anchor(
        &mut self,
        tx: &impl IrysTransactionCommon,
    ) -> Result<u64, TxIngressError> {
        let mempool_state = &self.mempool_state;
        let tx_id = tx.id();
        let anchor = tx.anchor();

        let read_tx = self.read_tx().map_err(|_| TxIngressError::DatabaseError)?;

        let latest_height = self.get_latest_block_height()?;
        let anchor_expiry_depth = self
            .config
            .node_config
            .consensus_config()
            .mempool
            .anchor_expiry_depth as u64;

        // check tree / mempool for block header
        if let Some(hdr) = self
            .mempool_state
            .read()
            .await
            .prevalidated_blocks
            .get(&anchor)
            .cloned()
        {
            if hdr.height + anchor_expiry_depth >= latest_height {
                debug!("valid block hash anchor for tx ");
                return Ok(hdr.height);
            } else {
                let mut mempool_state_write_guard = mempool_state.write().await;
                mempool_state_write_guard.recent_invalid_tx.put(tx_id, ());
                warn!(
                    "Invalid anchor value for tx - header height {} beyond expiry depth {}",
                    &hdr.height, &anchor_expiry_depth
                );
                return Err(TxIngressError::InvalidAnchor);
            }
        }

        // check index for block header
        match irys_database::block_header_by_hash(&read_tx, &anchor, false) {
            Ok(Some(hdr)) => {
                if hdr.height + anchor_expiry_depth >= latest_height {
                    debug!("valid block hash anchor for tx");
                    Ok(hdr.height)
                } else {
                    let mut mempool_state_write_guard = mempool_state.write().await;
                    mempool_state_write_guard.recent_invalid_tx.put(tx_id, ());
                    warn!("Invalid block hash anchor value for tx - header height {} beyond expiry depth {}", &hdr.height, &anchor_expiry_depth);
                    Err(TxIngressError::InvalidAnchor)
                }
            }
            _ => {
                let mut mempool_state_write_guard = mempool_state.write().await;
                mempool_state_write_guard.recent_invalid_tx.put(tx_id, ());
                warn!("Invalid anchor value for tx - Unknown anchor {}", &anchor);
                Err(TxIngressError::InvalidAnchor)
            }
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
            let tx_path = commitment_tx_path.join(format!("{}.json", tx.id.0.to_base58()));

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
            let tx_path = storage_tx_path.join(format!("{}.json", tx.id.0.to_base58()));

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

    // Helper to get the canonical chain and latest height
    fn get_latest_block_height(&self) -> Result<u64, TxIngressError> {
        let canon_chain = self.block_tree_read_guard.read().get_canonical_chain();
        let latest = canon_chain.0.last().ok_or(TxIngressError::Other(
            "unable to get canonical chain from block tree".to_owned(),
        ))?;

        Ok(latest.height)
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
    // /// Unknown anchor value (could be valid)
    // PendingAnchor,
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
    pub publish_tx: (Vec<DataTransactionHeader>, Vec<TxIngressProof>),
}
