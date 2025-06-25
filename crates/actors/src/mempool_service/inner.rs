use crate::block_tree_service::BlockTreeReadGuard;
use crate::mempool_service::types::{
    AtomicMempoolState, MempoolServiceMessage, MempoolTxs, TxIngressError,
};
use crate::services::ServiceSenders;
use crate::CommitmentStateReadGuard;
use base58::ToBase58 as _;
use futures::future::BoxFuture;
use irys_database::SystemLedger;
use irys_primitives::CommitmentType;
use irys_reth_node_bridge::{ext::IrysRethRpcTestContextExt as _, IrysRethNodeAdapter};
use irys_storage::{get_atomic_file, RecoveredMempoolState, StorageModulesReadGuard};
use irys_types::{
    app_state::DatabaseProvider, Config, IrysBlockHeader, IrysTransactionCommon, IrysTransactionId,
    H256, U256,
};
use reth::rpc::types::BlockId;
use reth::tasks::TaskExecutor;
use reth_db::{Database as _, DatabaseError};
use std::fs;
use std::io::Write as _;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub struct Inner {
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub commitment_state_guard: CommitmentStateReadGuard,
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
                    let response_value = self.handle_chunk_ingress_message(chunk).await;
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
            }
            Ok(())
        })
    }

    async fn handle_get_best_mempool_txs(
        &self,
        parent_evm_block_id: Option<BlockId>,
    ) -> MempoolTxs {
        let mempool_state = &self.mempool_state;
        let mut fees_spent_per_address = HashMap::new();
        let mut confirmed_commitments = HashSet::new();
        let mut commitment_tx = Vec::new();
        let mut unfunded_address = HashSet::new();

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

            let fee = tx.total_fee();
            let current_spent = *fees_spent_per_address.get(&signer).unwrap_or(&0_u64);

            // Calculate total required balance including previously selected transactions

            // get balance state for the block we're building off of
            let balance: U256 = self
                .reth_node_adapter
                .rpc
                .get_balance_irys(signer, parent_evm_block_id);

            let has_funds = balance >= U256::from(current_spent + fee);

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
        debug!(
            "best_mempool_txs: current head height {}",
            canonical.last().unwrap().height
        );

        // TODO: This approach should be applied to data TX and commitment TX should instead
        // be checked for prior inclusion using the Commitment State and current Commitment Snapshot
        for entry in canonical {
            let commitment_tx_ids = entry.system_ledgers.get(&SystemLedger::Commitment);
            if let Some(commitment_tx_ids) = commitment_tx_ids {
                for tx_id in &commitment_tx_ids.0 {
                    confirmed_commitments.insert(*tx_id);
                }
            }
        }

        // Process commitments in the mempool in priority order (stakes then pledges)
        // This order ensures stake transactions are processed before pledges
        let mempool_state_guard = mempool_state.read().await;

        for commitment_type in &[CommitmentType::Stake, CommitmentType::Pledge] {
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
            sorted_commitments.sort_by_key(|b| std::cmp::Reverse(b.total_fee()));

            // Select fundable commitments in fee-priority order
            for tx in sorted_commitments {
                if confirmed_commitments.contains(&tx.id) {
                    debug!(
                        "best_mempool_txs: skipping already confirmed commitment tx {}",
                        tx.id
                    );
                    continue; // Skip tx already confirmed in the canonical chain
                }
                if check_funding(&tx) {
                    debug!("best_mempool_txs: adding commitment tx {}", tx.id);
                    commitment_tx.push(tx);
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
        submit_ledger_txs.sort_by_key(|b| std::cmp::Reverse(b.total_fee()));

        // Apply block size constraint and funding checks to data transactions
        let mut submit_tx = Vec::new();
        let max_txs = self
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
            if check_funding(&tx) {
                submit_tx.push(tx);
                if submit_tx.len() >= max_txs {
                    break;
                }
            }
        }

        // Return selected transactions grouped by type
        MempoolTxs {
            commitment_tx,
            submit_tx,
        }
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
    pub async fn validate_anchor(
        &mut self,
        tx_id: &IrysTransactionId,
        anchor: &H256,
    ) -> Result<IrysBlockHeader, TxIngressError> {
        let mempool_state = &self.mempool_state;

        let read_tx = self.read_tx().map_err(|_| TxIngressError::DatabaseError)?;

        let latest_height = self.get_latest_block_height()?;
        let anchor_expiry_depth = self
            .config
            .node_config
            .consensus_config()
            .mempool
            .anchor_expiry_depth as u64;

        let mempool_state_read_guard = mempool_state.read().await;
        // Allow transactions to use the txid of a transaction in the mempool
        if mempool_state_read_guard.recent_valid_tx.contains(anchor) {
            let (canonical_blocks, _) = self.block_tree_read_guard.read().get_canonical_chain();
            let latest = canonical_blocks.last().unwrap();
            // Just provide the most recent block as an anchor
            if let Some(hdr) = mempool_state_read_guard
                .prevalidated_blocks
                .get(&latest.block_hash)
            {
                if hdr.height + anchor_expiry_depth >= latest_height {
                    debug!("valid txid anchor {} for tx {}", anchor, tx_id);
                    return Ok(hdr.clone());
                }
            } else if let Ok(Some(hdr)) =
                irys_database::block_header_by_hash(&read_tx, &latest.block_hash, false)
            {
                if hdr.height + anchor_expiry_depth >= latest_height {
                    debug!("valid txid anchor {} for tx {}", anchor, tx_id);
                    return Ok(hdr);
                }
            }
        }
        drop(mempool_state_read_guard); // Release read lock before acquiring write lock

        // check tree / mempool for block header
        if let Some(hdr) = self
            .mempool_state
            .read()
            .await
            .prevalidated_blocks
            .get(anchor)
            .cloned()
        {
            if hdr.height + anchor_expiry_depth >= latest_height {
                debug!("valid block hash anchor {} for tx {}", anchor, tx_id);
                return Ok(hdr);
            }
        }

        // check index for block header
        match irys_database::block_header_by_hash(&read_tx, anchor, false) {
            Ok(Some(hdr)) if hdr.height + anchor_expiry_depth >= latest_height => {
                debug!("valid block hash anchor {} for tx {}", anchor, tx_id);
                Ok(hdr)
            }
            _ => {
                let mut mempool_state_write_guard = mempool_state.write().await;
                mempool_state_write_guard.invalid_tx.push(*tx_id);
                warn!("Invalid anchor value {} for tx {}", anchor, tx_id);
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
    pub async fn validate_signature<T: IrysTransactionCommon>(
        &mut self,
        tx: &T,
    ) -> Result<(), TxIngressError> {
        if tx.is_signature_valid() {
            info!("Signature is valid");
            Ok(())
        } else {
            let mempool_state = &self.mempool_state;
            mempool_state.write().await.invalid_tx.push(tx.id());
            debug!("Signature is NOT valid");
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
