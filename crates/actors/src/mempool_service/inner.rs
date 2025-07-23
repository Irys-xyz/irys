use crate::block_discovery::get_data_tx_in_parallel_inner;
use crate::mempool_service::ChunkIngressError;
use crate::services::ServiceSenders;
use base58::ToBase58 as _;
use eyre::eyre;
use futures::future::BoxFuture;
use futures::FutureExt as _;
use irys_database::tables::IngressProofs;
use irys_database::{cached_data_root_by_data_root, SystemLedger};
use irys_domain::{BlockTreeReadGuard, CommitmentSnapshotStatus};
use irys_primitives::CommitmentType;
use irys_reth_node_bridge::{ext::IrysRethRpcTestContextExt as _, IrysRethNodeAdapter};
use irys_storage::{get_atomic_file, RecoveredMempoolState, StorageModulesReadGuard};
use irys_types::{
    app_state::DatabaseProvider, Config, IrysBlockHeader, IrysTransactionCommon, IrysTransactionId,
    H256, U256,
};
use irys_types::{
    Address, Base64, CommitmentTransaction, DataRoot, DataTransactionHeader, IrysTransaction,
    MempoolConfig, TxChunkOffset, TxIngressProof, UnpackedChunk,
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
use tracing::{debug, error, info, instrument, warn};

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
                unfunded_address.insert(signer);
                return false;
            }

            has_funds
        };

        // Get a list of all recently confirmed commitment txids in the canonical chain
        let (canonical, _) = self.block_tree_read_guard.read().get_canonical_chain();
        let last_block = canonical.last().unwrap();
        debug!(
            "best_mempool_txs: current head height {}",
            last_block.height
        );

        for entry in canonical.iter() {
            let commitment_tx_ids = entry.system_ledgers.get(&SystemLedger::Commitment);
            if let Some(commitment_tx_ids) = commitment_tx_ids {
                for tx_id in &commitment_tx_ids.0 {
                    confirmed_commitments.insert(*tx_id);
                }
            }
        }

        //create a throw away commitment snapshot so we can simulate behaviour before including a commitment tx in returned txs
        let mut simulation_commitment_snapshot = self
            .block_tree_read_guard
            .read()
            .canonical_commitment_snapshot()
            .as_ref()
            .clone();

        // Process commitments in the mempool in priority order (stakes then pledges)
        // This order ensures stake transactions are processed before pledges
        let mempool_state_guard = mempool_state.read().await;

        'outer: for commitment_type in &[CommitmentType::Stake, CommitmentType::Pledge] {
            // Gather all commitments of current type from all addresses
            let mut sorted_commitments: Vec<_> = mempool_state_guard
                .valid_commitment_tx
                .values()
                .flat_map(|txs| {
                    txs.iter()
                        .filter(|tx| tx.commitment_type == *commitment_type)
                        .cloned()
                })
                .collect();

            // Sort commitments by fee (highest first) to maximize network revenue
            sorted_commitments.sort_by_key(|b| std::cmp::Reverse(b.user_fee()));

            // Select fundable commitments in fee-priority order
            for tx in sorted_commitments {
                if confirmed_commitments.contains(&tx.id) {
                    debug!(
                        "best_mempool_txs: skipping already confirmed commitment tx {}",
                        tx.id
                    );
                    continue; // Skip tx already confirmed in the canonical chain
                }

                // Check funding before simulation so we don't mutate the snapshot unnecessarily
                if !check_funding(&tx) {
                    continue;
                }

                // signer stake status check
                if tx.commitment_type == CommitmentType::Stake {
                    let epoch_snapshot = self
                        .block_tree_read_guard
                        .read()
                        .get_epoch_snapshot(&last_block.block_hash)
                        .expect("parent blocks epoch_snapshot should be retrievable");
                    let is_staked = epoch_snapshot.is_staked(tx.signer);
                    tracing::error!(
                        "tx.id: {:?} tx.signer {:?} is_staked: {:?}",
                        tx.id,
                        tx.signer,
                        is_staked
                    );
                    if is_staked {
                        // if a signer has stake commitments in the mempool, but is already staked, we should ignore them
                        continue;
                    }
                }
                // simulation check
                {
                    let is_staked = self
                        .block_tree_read_guard
                        .read()
                        .canonical_epoch_snapshot()
                        .is_staked(tx.signer);

                    let simulation = simulation_commitment_snapshot.add_commitment(&tx, is_staked);

                    // skip commitments that would not be accepted
                    if simulation != CommitmentSnapshotStatus::Accepted {
                        tracing::error!(
                            "tx {:?}:{:?} skipped: {:?}",
                            tx.commitment_type,
                            tx.id,
                            simulation
                        );
                        continue;
                    }
                }

                debug!("best_mempool_txs: adding commitment tx {}", tx.id);
                commitment_tx.push(tx);

                // if we have reached the maximum allowed number of commitment txs per block
                // do not push anymore
                if commitment_tx.len() >= max_commitments {
                    break 'outer;
                }
            }
        }
        drop(mempool_state_guard);

        debug!(
            "best_mempool_txs: confirmed_commitments\n {:#?}",
            confirmed_commitments
        );
        debug!(
            "best_mempool_txs: best commitments \n {:#?}",
            commitment_tx
                .iter()
                .map(|t| (t.id, t.commitment_type))
                .collect::<Vec<_>>()
        );

        // Prepare data transactions for inclusion after commitments
        let mut submit_ledger_txs = self.get_pending_submit_ledger_txs().await;

        // Sort data transactions by fee (highest first) to maximize revenue

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
            debug!("Checking funding for {}", &tx.id);
            if check_funding(&tx) {
                debug!("Submit tx {} passed the funding check", &tx.id);
                submit_tx.push(tx);
                if submit_tx.len() >= max_data_txs {
                    break;
                }
            } else {
                debug!("Submit tx {} failed the funding check", &tx.id)
            }
        }

        // note: publish txs are sorted internally by the get_publish_txs_and_proofs fn
        let publish_txs_and_proofs = self.get_publish_txs_and_proofs().await?;

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
        let mut proofs: Vec<TxIngressProof> = Vec::new();

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
                    // Get the proof
                    match ingress_proofs.get(&tx_header.data_root) {
                        Some(proof) => {
                            let mut tx_header = tx_header.clone();
                            let proof: TxIngressProof = TxIngressProof {
                                proof: proof.proof,
                                signature: proof.signature,
                            };
                            debug!(
                                "Got ingress proof {} for publish candidate {}",
                                &tx_header.data_root, &tx_header.id
                            );
                            proofs.push(proof.clone());
                            tx_header.ingress_proofs = Some(proof);
                            publish_txs.push(tx_header);
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
        Ok((publish_txs, proofs))
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
    // this takes in an IrysTransaction, and will pass it into `pending_anchor_txs` if it fails validation
    // otherwise, returns the tx back with the height that made the anchor canonical (i.e the block height, or the height of the block containing the tx)
    #[instrument(skip_all, fields(tx_id = %tx.id(), anchor = %tx.anchor()))]
    pub async fn validate_anchor(
        &mut self,
        tx: IrysTransaction,
    ) -> Result<Option<(u64, IrysTransaction)>, TxIngressError> {
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

        // try to find the anchor tx in the block tree
        {
            let canonical_chain = {
                let block_tree_read_guard = self.block_tree_read_guard.read();
                let (canonical_chain, _) = block_tree_read_guard.get_canonical_chain();
                canonical_chain
            };

            let canonical_slice = &canonical_chain
            // usize coercion safe as it's actually a u8
                [(canonical_chain.len().saturating_sub(anchor_expiry_depth as usize))..canonical_chain.len()];

            for blk_entry in canonical_slice.iter().rev() {
                // go from newest block to oldest

                for txs in blk_entry
                    .data_ledgers
                    .values()
                    .chain(blk_entry.system_ledgers.values())
                {
                    if txs.contains(&anchor) {
                        return Ok(Some((blk_entry.height, tx)));
                    }
                }
            }
        }

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
                return Ok(Some((hdr.height, tx)));
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
                    Ok(Some((hdr.height, tx)))
                } else {
                    let mut mempool_state_write_guard = mempool_state.write().await;
                    mempool_state_write_guard.recent_invalid_tx.put(tx_id, ());
                    warn!("Invalid block hash anchor value for tx - header height {} beyond expiry depth {}", &hdr.height, &anchor_expiry_depth);
                    Err(TxIngressError::InvalidAnchor)
                }
            }
            _ => {
                // we mark the tx as pending it's anchor,
                // given it passed through all our checks and we couldn't find what the anchor value was referring to
                // TODO: plumb success context (the tx being marked as "pending anchor" isn't an error case, but we don't have a way of signaling that yet)
                self.mark_tx_pending_anchor(tx)
                    .await
                    .map_err(|_e| TxIngressError::InvalidAnchor)
                    .map(|_| None)
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
            info!("Signature is valid");
            Ok(())
        } else {
            let mempool_state = &self.mempool_state;
            mempool_state
                .write()
                .await
                .recent_invalid_tx
                .put(tx.id(), ());
            warn!("Signature is invalid");
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

    /// Mark the provided tx as waiting for it's anchor to be valid
    #[instrument(skip_all)]
    pub async fn mark_tx_pending_anchor(&self, tx: IrysTransaction) -> eyre::Result<()> {
        let tx_id = tx.id();
        debug!(tx_id = ?tx_id, anchor = ?tx.anchor(), "Tx is pending anchor");
        let mut state_rw = self.mempool_state.write().await;
        let anchor = tx.anchor();
        match state_rw.pending_anchor_txs.push(tx.id(), tx) {
            // if we have removed an old entry, and not updated an existing one
            Some((k, _v)) if k != anchor => {
                // remove this entry from the pending_anchor_map
                match state_rw.pending_anchor_txids.get_mut(&k) {
                    Some(hs) => {
                        debug!("Evicting {:?} from pending_anchor_txs LRUs", &k);
                        hs.remove(&k)
                    }
                    None => false,
                };
            }
            // successfully updated/added this entry
            // TODO: warn if we've updated? we should never do so.
            Some(_) | None => {}
        };
        // add the tx to `pending_anchor_map`
        state_rw
            .pending_anchor_txids
            .entry(anchor)
            .or_default()
            .insert(tx_id);

        Ok(())
    }

    #[instrument(skip_all)]
    // notify the pending_anchor_txs cache of a new anchor value
    pub async fn notify_anchor(&mut self, anchor: H256) {
        debug!("New anchor {:?}", &anchor);
        // check if we have any pending txs waiting for this anchor
        let waiting_for_anchor: HashSet<H256> = {
            let mut state_rw = self.mempool_state.write().await;
            // only continue if we get some pending ids
            // (also remove the entry from pending_anchor_map)
            match state_rw.pending_anchor_txids.remove(&anchor) {
                Some(waiting) if !waiting.is_empty() => waiting,
                Some(_) | None => return,
            }
        };

        // throw all pending txs back to the mempool for re-validation
        for id in waiting_for_anchor {
            let tx = {
                // make sure write lock is scoped - as the handle fns need write access
                let mut state_rw = self.mempool_state.write().await;
                // pop for 1.) cleanup, 2.) no lifetime nonsense
                state_rw.pending_anchor_txs.pop(&id)
            };
            let tx = match tx {
                Some(tx) => tx,
                None => {
                    warn!(
                        "Unable to resubmit tx {} with anchor {} - not found in pending_anchor_txs",
                        &id, &anchor
                    );
                    continue;
                }
            };

            debug!("Re-validating pending tx {} due to anchor {}", &id, &anchor);

            match match tx {
                IrysTransaction::Data(tx) => self.handle_data_tx_ingress_message(tx).await,
                IrysTransaction::Commitment(tx) => {
                    self.handle_ingress_commitment_tx_message(tx).await
                }
            } {
                Ok(_) => debug!(
                    "re-submitted tx {} to mempool after anchor {} became available",
                    id, &anchor,
                ),
                Err(err) => warn!(
                    "failed to re-submit tx {} to mempool after anchor {} became available - {:?}",
                    id, &anchor, &err
                ),
            }
        }
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
    // maps tx_id -> tx
    // TODO: add secondary time-based pruning behaviour? see what occupancy looks like.
    pub pending_anchor_txs: LruCache<IrysTransactionId, IrysTransaction>,
    // maps anchor value -> tx_ids that are waiting for it
    pub pending_anchor_txids: HashMap<H256, HashSet<IrysTransactionId>>,
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(config: &MempoolConfig) -> MempoolState {
    let max_pending_chunk_items = config.max_pending_chunk_items;
    let max_pending_pledge_items = config.max_pending_pledge_items;
    let max_pending_anchor_items = config.max_pending_anchor_items;
    MempoolState {
        prevalidated_blocks: HashMap::new(),
        prevalidated_blocks_poa: HashMap::new(),
        valid_submit_ledger_tx: BTreeMap::new(),
        valid_commitment_tx: BTreeMap::new(),
        recent_invalid_tx: LruCache::new(NonZeroUsize::new(config.max_invalid_items).unwrap()),
        recent_valid_tx: LruCache::new(NonZeroUsize::new(config.max_valid_items).unwrap()),
        pending_chunks: LruCache::new(NonZeroUsize::new(max_pending_chunk_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
        pending_anchor_txs: LruCache::new(NonZeroUsize::new(max_pending_anchor_items).unwrap()),
        pending_anchor_txids: HashMap::new(),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    Unfunded,
    /// This transaction id is already in the cache
    Skipped,
    /// Invalid anchor value (unknown or too old)
    InvalidAnchor,
    // /// Unknown anchor value (could be valid)
    // PendingAnchor,
    /// Some database error occurred
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    Other(String),
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
