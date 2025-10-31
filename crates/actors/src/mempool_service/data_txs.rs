use crate::mempool_service::{Inner, TxReadError};
use crate::mempool_service::{MempoolServiceMessage, TxIngressError};
use eyre::eyre;
use irys_database::{
    block_header_by_hash, db::IrysDatabaseExt as _, tables::CachedDataRoots, tx_header_by_txid,
};
use irys_domain::get_optimistic_chain;
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_types::storage_pricing::{
    calculate_perm_fee_from_config, calculate_term_fee,
    phantoms::Percentage,
    Decimal,
};
use irys_types::TxKnownStatus;
use irys_types::{
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    DataLedger, DataTransactionHeader, GossipBroadcastMessage, IrysTransactionCommon as _,
    IrysTokenPrice, IrysTransactionId, H256, U256,
};
use reth_db::transaction::DbTxMut as _;
use reth_db::Database as _;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

impl Inner {
    // Shared pre-checks for both API and Gossip data tx ingress paths.
    // Performs duplicate detection, signature validation, anchor validation, expiry computation,
    // and ledger parsing. Returns the resolved ledger and the computed expiry height.
    #[inline]
    async fn precheck_data_ingress_common(
        &mut self,
        tx: &DataTransactionHeader,
    ) -> Result<(DataLedger, u64), TxIngressError> {
        // Fast-fail if we've recently seen this exact invalid payload (by signature fingerprint)
        {
            // Compute composite fingerprint: keccak(signature + prehash + id)
            // TODO: share the signature hash computed here with validate_signature
            let fingerprint = tx.fingerprint();
            if self
                .mempool_state
                .read()
                .await
                .recent_invalid_payload_fingerprints
                .contains(&fingerprint)
            {
                return Err(TxIngressError::InvalidSignature);
            }
        }
        // Early exit if already known in mempool or DB
        {
            let tx_status = self
                .handle_data_tx_exists_message(tx.id)
                .await
                .map_err(|e| {
                    TxIngressError::Other(format!("DB error checking known tx: {:?}", e))
                })?;
            if tx_status.is_known_and_valid() {
                return Err(TxIngressError::Skipped);
            }
        }

        // Validate signature
        self.validate_signature(tx).await?;

        // Validate anchor and compute expiry
        let anchor_height = self.validate_anchor(tx).await?;
        let expiry_height = self.compute_expiry_height_from_anchor(anchor_height);

        // Validate and parse ledger type
        let ledger = self.parse_ledger(tx)?;

        Ok((ledger, expiry_height))
    }

    // Shared post-processing: insert into mempool, cache data_root with expiry,
    // process any pending chunks, and gossip the transaction.
    #[inline]
    async fn postprocess_data_ingress(
        &mut self,
        tx: &DataTransactionHeader,
        expiry_height: u64,
    ) -> Result<(), TxIngressError> {
        self.insert_tx_and_mark_valid(tx).await;
        self.cache_data_root_with_expiry(tx, expiry_height);
        self.process_pending_chunks_for_root(tx.data_root).await?;
        self.broadcast_tx_gossip(tx);
        Ok(())
    }
    /// check the mempool and mdbx for data transaction
    /// TODO: align the logic with handle_get_commitment_tx_message (specifically HashMap output)
    pub async fn handle_get_data_tx_message(
        &self,
        txs: Vec<H256>,
    ) -> Vec<Option<DataTransactionHeader>> {
        let mut found_txs = Vec::with_capacity(txs.len());
        let mempool_state_guard = self.mempool_state.read().await;

        for tx in txs {
            // if data tx exists in mempool
            if let Some(tx_header) = mempool_state_guard.valid_submit_ledger_tx.get(&tx) {
                debug!("Got tx {} from mempool", &tx);
                found_txs.push(Some(tx_header.clone()));
                continue;
            }

            // if data tx exists in mdbx
            match self.read_tx() {
                Ok(read_tx) => match tx_header_by_txid(&read_tx, &tx) {
                    Ok(Some(tx_header)) => {
                        debug!("Got tx {} from DB", &tx);
                        found_txs.push(Some(tx_header.clone()));
                        continue;
                    }
                    Ok(None) => {
                        debug!("Tx {} not found in DB", &tx);
                    }
                    Err(e) => {
                        warn!("DB error reading tx {}: {}", &tx, e);
                    }
                },
                Err(e) => {
                    warn!("Failed to open DB read transaction: {}", e);
                }
            }
            // not found anywhere
            found_txs.push(None);
        }

        drop(mempool_state_guard);
        found_txs
    }

    pub async fn handle_data_tx_ingress_message_gossip(
        &mut self,
        tx: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx.id = ?tx.id,
            tx.data_root = ?tx.data_root,
            "Received data tx from Gossip"
        );

        // preserving promoted_height value on ingress is the safest policy
        // mutating on ingress would allow for various incorrect behaviours such as skipping already-promoted txs by consulting this flag
        // this allows proper chain-handling flows to adjust it if needed (e.g., on a reorg event)
        if tx.promoted_height.is_some() {
            warn!(
                "Ingressed tx {:?} has promoted_height set to {:?}; preserving existing promotion state",
                tx.id,
                tx.promoted_height
            );
        }

        // Shared pre-checks: duplicate detection, signature, anchor/expiry, ledger parsing
        let (ledger, expiry_height) = self.precheck_data_ingress_common(&tx).await?;

        // Protocol fee structure checks (Gossip: skip)
        //
        // Rationale:
        // - When we receive a gossiped tx, it may belong to a different fork with a different
        //   EMA/pricing context. To avoid false rejections, we limit validation for Gossip
        //   sources to signature + anchor checks only (performed above), and skip fee structure
        //   checks here.
        // - Similarly, we skip balance and EMA pricing validation for gossip, as these are
        //   canonical-chain-specific and may differ across forks.
        match ledger {
            DataLedger::Publish => {
                // Gossip path: skip API-only checks here
            }
            DataLedger::Submit => {
                // Submit ledger - a data transaction cannot target the submit ledger directly
                return Err(TxIngressError::InvalidLedger(ledger as u32));
            }
        }

        // Shared post-processing
        self.postprocess_data_ingress(&tx, expiry_height).await
    }

    pub async fn handle_data_tx_ingress_message_api(
        &mut self,
        mut tx: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx.id = ?tx.id,
            tx.data_root = ?tx.data_root,
            "Received data tx from API"
        );

        tx.promoted_height = None;

        // Shared pre-checks: duplicate detection, signature, anchor/expiry, ledger parsing
        let (ledger, expiry_height) = self.precheck_data_ingress_common(&tx).await?;

        // Validate funding against canonical chain (API only)
        // Note: We do NOT mark tx as invalid on funding failure, allowing
        // it to be revalidated if the account is funded later
        self.validate_data_tx_funding(&tx)?;

        // Protocol fee structure checks (API only)
        //
        // Rationale:
        // - When a user submits a tx via our API, we validate balance, EMA pricing, and fee
        //   structure against our canonical view so the user gets immediate feedback if it's
        //   malformed/underfunded.
        // - When we receive a gossiped tx, it may belong to a different fork with a different
        //   EMA/pricing context or account balance state. To avoid false rejections, we limit
        //   validation for Gossip sources to signature + anchor checks only (performed above),
        //   and skip balance/fee checks here.
        match ledger {
            DataLedger::Publish => {
                // Publish ledger - permanent storage
                self.validate_fee_structure_api_only(&tx)?;
            }
            DataLedger::Submit => {
                // Submit ledger - a data transaction cannot target the submit ledger directly
                return Err(TxIngressError::InvalidLedger(ledger as u32));
            }
        }

        // Validate fees against maximum EMA pricing (API only, strict - no tolerance)
        // This provides immediate feedback to API users if their transaction fees are insufficient
        self.validate_data_tx_ema_pricing(&tx, Decimal::ZERO, false)
            .await?;

        // Shared post-processing
        self.postprocess_data_ingress(&tx, expiry_height).await
    }

    // --- Small shared helpers (kept private to this module) ---

    /// Validates that a data transaction has sufficient balance to cover its fees.
    /// Checks the balance against the canonical chain tip.
    ///
    /// IMPORTANT: This function does NOT mark the transaction as invalid if unfunded,
    /// allowing the transaction to be revalidated later if the account is funded.
    fn validate_data_tx_funding(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        // Fetch balance from canonical chain (None = canonical tip)
        let balance: U256 = self
            .reth_node_adapter
            .rpc
            .get_balance_irys_canonical_and_pending(tx.signer, None)
            .map_err(|e| {
                tracing::error!(
                    tx.id = %tx.id,
                    tx.signer = %tx.signer,
                    tx.error = %e,
                    "Failed to fetch balance for data tx"
                );
                TxIngressError::BalanceFetchError {
                    address: tx.signer.to_string(),
                    reason: e.to_string(),
                }
            })?;

        let required = tx.total_cost();

        if balance < required {
            tracing::warn!(
                tx.id = %tx.id,
                account.balance = %balance,
                tx.required_balance = %required,
                tx.signer = %tx.signer,
                "Insufficient balance for data tx"
            );
            return Err(TxIngressError::Unfunded);
        }

        tracing::debug!(
            tx.id = %tx.id,
            account.balance = %balance,
            tx.required_balance = %required,
            "Funding validated for data tx"
        );

        Ok(())
    }

    /// Get the maximum EMA price from the canonical tip's snapshot and calculate
    /// the minimum acceptable price with the given tolerance.
    ///
    /// Returns: (max_ema, min_acceptable_ema)
    /// - max_ema: The highest of the three EMA interval prices
    /// - min_acceptable_ema: max_ema reduced by tolerance% (or max_ema if tolerance is 0)
    fn get_pricing_bounds(
        &self,
        tolerance_percent: Decimal,
    ) -> Result<(IrysTokenPrice, IrysTokenPrice), TxIngressError> {
        let ema_snapshot = {
            let tree = self.block_tree_read_guard.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block = canonical
                .last()
                .ok_or_else(|| TxIngressError::Other("Empty canonical chain".to_string()))?;
            tree.get_ema_snapshot(&last_block.block_hash)
                .ok_or_else(|| TxIngressError::Other("EMA snapshot not found".to_string()))?
        };

        // Find MAX of the three EMA prices
        let max_ema = [
            ema_snapshot.ema_price_2_intervals_ago,
            ema_snapshot.ema_price_1_interval_ago,
            ema_snapshot.ema_price_current_interval,
        ]
        .into_iter()
        .max_by_key(|price| price.amount)
        .ok_or_else(|| TxIngressError::Other("Failed to find max EMA".to_string()))?;

        // Calculate minimum acceptable price: max_ema * (1 - tolerance%)
        let min_ema = if tolerance_percent > Decimal::ZERO {
            let tolerance = irys_types::storage_pricing::Amount::<Percentage>::percentage(tolerance_percent)
                .map_err(|e| TxIngressError::Other(format!("Invalid tolerance: {}", e)))?;
            max_ema
                .sub_multiplier(tolerance)
                .map_err(|e| TxIngressError::Other(format!("Failed to calculate min EMA: {}", e)))?
        } else {
            max_ema // No tolerance for API
        };

        Ok((max_ema, min_ema))
    }

    /// Validates data transaction fees against the MAXIMUM of all EMA interval prices.
    /// Uses MAX(ema_2_intervals_ago, ema_1_interval_ago, ema_current) to ensure
    /// fees are sufficient regardless of which pricing EMA applies.
    ///
    /// # Parameters
    /// - `tolerance_percent`: Allowed deviation from expected fee (e.g., 15.0 for ±15%)
    ///   - API uses 0.0 (strict validation)
    ///   - Gossip uses 15.0 (lenient, to accommodate cross-fork pricing differences)
    /// - `mark_invalid_on_failure`: Whether to add to invalid tx list on failure (true for Gossip)
    async fn validate_data_tx_ema_pricing(
        &mut self,
        tx: &DataTransactionHeader,
        tolerance_percent: Decimal,
        mark_invalid_on_failure: bool,
    ) -> Result<(), TxIngressError> {
        let (max_ema, min_ema) = self.get_pricing_bounds(tolerance_percent)?;

        // Calculate expected fees using the MINIMUM acceptable EMA price
        // This means we calculate fees at (max_ema - tolerance%), making the validation lenient
        let latest_height = self.get_latest_block_height()?;
        let next_block_height = latest_height + 1;
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            next_block_height,
            self.config.consensus.epoch.num_blocks_in_epoch,
            self.config.consensus.epoch.submit_ledger_epoch_length,
        );

        let expected_term_fee = calculate_term_fee(
            tx.data_size,
            epochs_for_storage,
            &self.config.consensus,
            min_ema,
        )
        .map_err(|e| TxIngressError::Other(format!("Failed to calculate term fee: {}", e)))?;

        // Validate term fee
        if tx.term_fee < expected_term_fee {
            let err_msg = format!(
                "Insufficient term fee for max EMA pricing (tolerance={}%)",
                tolerance_percent
            );

            if mark_invalid_on_failure {
                let state = self.mempool_state.write().await;
                Self::mark_tx_as_invalid(state, tx.id, &err_msg);
            }

            tracing::warn!(
                tx.id = %tx.id,
                tx.term_fee = %tx.term_fee,
                expected_min = %expected_term_fee,
                max_ema = %max_ema.amount,
                min_ema = %min_ema.amount,
                tolerance = %tolerance_percent,
                "Data tx insufficient term_fee"
            );

            return Err(TxIngressError::Other(err_msg));
        }

        // For Publish ledger, validate perm fee
        if let Ok(DataLedger::Publish) = DataLedger::try_from(tx.ledger_id) {
            let perm_fee = tx.perm_fee.ok_or_else(|| {
                TxIngressError::Other("Publish tx missing perm_fee".to_string())
            })?;

            let expected_perm_fee = calculate_perm_fee_from_config(
                tx.data_size,
                &self.config.consensus,
                min_ema,
                expected_term_fee,
            )
            .map_err(|e| TxIngressError::Other(format!("Failed to calculate perm fee: {}", e)))?;

            if perm_fee < expected_perm_fee.amount {
                let err_msg = format!(
                    "Insufficient perm fee for max EMA pricing (tolerance={}%)",
                    tolerance_percent
                );

                if mark_invalid_on_failure {
                    let state = self.mempool_state.write().await;
                    Self::mark_tx_as_invalid(state, tx.id, &err_msg);
                }

                tracing::warn!(
                    tx.id = %tx.id,
                    tx.perm_fee = %perm_fee,
                    expected_min = %expected_perm_fee.amount,
                    max_ema = %max_ema.amount,
                    min_ema = %min_ema.amount,
                    tolerance = %tolerance_percent,
                    "Data tx insufficient perm_fee"
                );

                return Err(TxIngressError::Other(err_msg));
            }
        }

        tracing::debug!(
            tx.id = %tx.id,
            max_ema = %max_ema.amount,
            min_ema = %min_ema.amount,
            tolerance = %tolerance_percent,
            "Data tx EMA pricing validated"
        );

        Ok(())
    }

    /// Computes the pre-confirmation expiry height given a resolved anchor height.
    fn compute_expiry_height_from_anchor(&self, anchor_height: u64) -> u64 {
        let anchor_expiry_depth = self.config.consensus.mempool.anchor_expiry_depth as u64;
        anchor_height + anchor_expiry_depth
    }

    /// Parses the ledger id from the tx and maps errors to TxIngressError.
    fn parse_ledger(&self, tx: &DataTransactionHeader) -> Result<DataLedger, TxIngressError> {
        DataLedger::try_from(tx.ledger_id)
            .map_err(|_err| TxIngressError::InvalidLedger(tx.ledger_id))
    }

    /// Inserts tx into the mempool and marks it as recently valid.
    async fn insert_tx_and_mark_valid(&mut self, tx: &DataTransactionHeader) {
        let mut guard = self.mempool_state.write().await;
        guard.valid_submit_ledger_tx.insert(tx.id, tx.clone());
        guard.recent_valid_tx.put(tx.id, ());
    }

    /// Caches data_root with expiry, logging success/failure.
    fn cache_data_root_with_expiry(&self, tx: &DataTransactionHeader, expiry_height: u64) {
        match self.irys_db.update_eyre(|db_tx| {
            let mut cdr = irys_database::cache_data_root(db_tx, tx, None)?
                .ok_or_else(|| eyre!("failed to cache data_root"))?;
            cdr.expiry_height = Some(expiry_height);
            db_tx.put::<CachedDataRoots>(tx.data_root, cdr)?;
            Ok(())
        }) {
            Ok(()) => {
                info!(
                    "Successfully cached data_root {:?} for tx {:?}",
                    tx.data_root, tx.id
                );
            }
            Err(db_error) => {
                error!(
                    "Failed to cache data_root {:?} for tx {:?}: {:?}",
                    tx.data_root, tx.id, db_error
                );
            }
        };
    }

    /// Processes any pending chunks that arrived before their parent transaction.
    async fn process_pending_chunks_for_root(
        &mut self,
        data_root: H256,
    ) -> Result<(), TxIngressError> {
        let mut guard = self.mempool_state.write().await;
        let option_chunks_map = guard.pending_chunks.pop(&data_root);
        drop(guard);

        if let Some(chunks_map) = option_chunks_map {
            let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
            for chunk in chunks {
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                if let Err(e) = self
                    .handle_message(MempoolServiceMessage::IngestChunk(chunk, oneshot_tx))
                    .await
                {
                    warn!("Failed to send chunk to mempool: {:?}", e);
                }

                let msg_result = oneshot_rx
                    .await
                    .expect("pending chunks should be processed by the mempool");

                if let Err(err) = msg_result {
                    tracing::error!("oneshot failure: {:?}", err);
                    return Err(TxIngressError::Other("oneshot failure".to_owned()));
                }
            }
        }
        Ok(())
    }

    /// Broadcasts the transaction over gossip, with error logging.
    fn broadcast_tx_gossip(&self, tx: &DataTransactionHeader) {
        let gossip_broadcast_message = GossipBroadcastMessage::from(tx.clone());
        if let Err(error) = self
            .service_senders
            .gossip_broadcast
            .send(gossip_broadcast_message)
        {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }
    }

    /// API-only validation of fee distribution structures for Publish ledger.
    fn validate_fee_structure_api_only(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let actual_perm_fee = tx.perm_fee.ok_or(TxIngressError::Other(
            "Perm fee must be present".to_string(),
        ))?;

        let actual_term_fee = tx.term_fee;

        TermFeeCharges::new(actual_term_fee, &self.config.node_config.consensus_config())
            .map_err(|e| TxIngressError::Other(format!("Invalid term fee structure: {}", e)))?;

        PublishFeeCharges::new(
            actual_perm_fee,
            actual_term_fee,
            &self.config.node_config.consensus_config(),
        )
        .map_err(|e| TxIngressError::Other(format!("Invalid perm fee structure: {}", e)))?;

        Ok(())
    }

    pub async fn get_all_storage_tx(&self) -> HashMap<IrysTransactionId, DataTransactionHeader> {
        let mut hash_map = HashMap::new();

        // first flat_map all the storage transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // Get any DataTransaction from the valid storage txs
        mempool_state_guard
            .valid_submit_ledger_tx
            .values()
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
            });

        hash_map
    }

    /// checks mempool and mdbx
    pub async fn handle_data_tx_exists_message(
        &self,
        txid: H256,
    ) -> Result<TxKnownStatus, TxReadError> {
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // #[expect(clippy::if_same_then_else, reason = "readability")]
        if mempool_state_guard
            .valid_submit_ledger_tx
            .contains_key(&txid)
        {
            Ok(TxKnownStatus::Valid)
        } else if mempool_state_guard.recent_valid_tx.contains(&txid) {
            Ok(TxKnownStatus::ValidSeen)
        } else if mempool_state_guard.recent_invalid_tx.contains(&txid) {
            // Still has it, just invalid
            Ok(TxKnownStatus::InvalidSeen)
        } else {
            drop(mempool_state_guard);
            let read_tx = self.read_tx();

            if read_tx.is_err() {
                Err(TxReadError::DatabaseError)
            } else if tx_header_by_txid(&read_tx.expect("expected valid header from tx id"), &txid)
                .map_err(|_| TxReadError::DatabaseError)?
                .is_some()
            {
                Ok(TxKnownStatus::Migrated)
            } else {
                Ok(TxKnownStatus::Unknown)
            }
        }
    }

    /// Returns all Submit ledger transactions that are pending inclusion in future blocks.
    ///
    /// This function specifically filters the Submit ledger mempool to exclude transactions
    /// that have already been included in recent canonical blocks within the anchor expiry
    /// window. Unlike the general mempool filter, this focuses solely on Submit transactions.
    ///
    /// # Algorithm
    /// 1. Starts with all valid Submit ledger transactions from mempool
    /// 2. Walks backwards through canonical chain within anchor expiry depth
    /// 3. Removes Submit transactions that already exist in historical blocks
    /// 4. Returns remaining pending Submit transactions
    ///
    /// # Returns
    /// A vector of `DataTransactionHeader` representing Submit ledger transactions
    /// that are pending inclusion and have not been processed in recent blocks.
    ///
    /// # Notes
    /// - Only considers Submit ledger transactions (filters out Publish, etc.)
    /// - Only examines blocks within the configured `anchor_expiry_depth`
    pub async fn get_pending_submit_ledger_txs(&self) -> Vec<DataTransactionHeader> {
        // Get the current canonical chain head to establish our starting point for block traversal
        // TODO: `get_optimistic_chain` and `get_canonical_chain` can be 2 different entries!
        let optimistic = get_optimistic_chain(self.block_tree_read_guard.clone())
            .await
            .unwrap();
        let (canonical, _) = self.block_tree_read_guard.read().get_canonical_chain();
        let canonical_head_entry = canonical.last().unwrap();

        // This is just here to catch any oddities in the debug log. The optimistic
        // and canonical should always have the same results from my reading of the code.
        // if the tests are stable and this hasn't come up it can be removed.
        if optimistic.last().unwrap().0 != canonical_head_entry.block_hash {
            debug!("Optimistic and Canonical have different heads");
        }

        let block_hash = canonical_head_entry.block_hash;
        let block_height = canonical_head_entry.height;

        // retrieve block from mempool or database
        // be aware that genesis starts its life immediately in the database
        let mut block = match self
            .handle_get_block_header_message(block_hash, false)
            .await
        {
            Some(b) => b,
            None => match self
                .irys_db
                .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))
            {
                Ok(Some(header)) => Ok(header),
                Ok(None) => Err(eyre!(
                    "No block header found for hash {} ({})",
                    block_hash,
                    block_height
                )),
                Err(e) => Err(eyre!(
                    "Failed to get previous block ({}) header: {}",
                    block_height,
                    e
                )),
            }
            .expect("to find the block header in the db"),
        };

        // Calculate the minimum block height we need to check for transaction conflicts
        // Only transactions anchored within this depth window are considered valid
        let anchor_expiry_depth = self.config.consensus.mempool.anchor_expiry_depth as u64;
        let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);

        // Start with all valid Submit ledger transactions - we'll filter out already-included ones
        let mut valid_submit_ledger_tx = self
            .mempool_state
            .read()
            .await
            .valid_submit_ledger_tx
            .clone();

        // Walk backwards through the canonical chain, removing Submit transactions
        // that have already been included in recent blocks within the anchor expiry window
        while block.height >= min_anchor_height {
            let block_data_tx_ids = block.get_data_ledger_tx_ids();

            // Check if this block contains any Submit ledger transactions
            if let Some(submit_txids) = block_data_tx_ids.get(&DataLedger::Submit) {
                // Remove Submit transactions that already exist in this historical block
                // This prevents double-inclusion and ensures we only return truly pending transactions
                for txid in submit_txids.iter() {
                    valid_submit_ledger_tx.remove(txid);
                }
            }

            // Stop if we've reached the genesis block
            if block.height == 0 {
                break;
            }

            // Move to the parent block and continue the traversal backwards
            let parent_block = match self
                .handle_get_block_header_message(block.previous_block_hash, false)
                .await
            {
                Some(h) => h,
                None => self
                    .irys_db
                    .view(|tx| {
                        irys_database::block_header_by_hash(tx, &block.previous_block_hash, false)
                    })
                    .unwrap()
                    .unwrap()
                    .expect("to find the parent block header in the database"),
            };

            block = parent_block;
        }

        // Return all remaining Submit transactions by consuming the map
        // These represent Submit transactions that are pending and haven't been included in any recent block
        valid_submit_ledger_tx.into_values().collect()
    }
}
