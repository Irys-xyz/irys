use crate::chunk_ingress_service::ChunkIngressMessage;
use crate::mempool_service::TxIngressError;
use crate::mempool_service::{Inner, TxReadError};
use crate::metrics;
use eyre::eyre;
use irys_database::{
    block_header_by_hash, db::IrysDatabaseExt as _, tables::CachedDataRoots, tx_header_by_txid,
};
use irys_domain::{HardforkConfigExt as _, get_optimistic_chain};
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_types::TxKnownStatus;
use irys_types::storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee};
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    DataLedger, DataTransactionHeader, H256, IrysTransactionCommon as _, IrysTransactionId,
    SendTraced as _, U256,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
};
use reth_db::Database as _;
use reth_db::transaction::DbTxMut as _;
use std::collections::HashMap;
use tracing::{debug, error, info, trace, warn};

impl Inner {
    // Shared pre-checks for both API and Gossip data tx ingress paths.
    // Performs duplicate detection, signature validation, anchor validation, expiry computation,
    // and ledger parsing. Returns the resolved ledger and the computed expiry height.
    #[inline]
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_root = ?tx.data_root))]
    async fn precheck_data_ingress_common(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(DataLedger, u64), TxIngressError> {
        // Fast-fail if this tx targets an unsupported ledger
        let ledger = DataLedger::try_from(tx.ledger_id)
            .map_err(|_| TxIngressError::InvalidLedger(tx.ledger_id))?;
        if !ledger.is_user_targetable() {
            return Err(TxIngressError::InvalidLedger(tx.ledger_id));
        }
        if matches!(ledger, DataLedger::OneYear | DataLedger::ThirtyDay) {
            // Valid only when Cascade hardfork is active (epoch-aligned activation)
            let cascade_active = {
                let tree = self.block_tree_read_guard.read();
                let epoch_snapshot = tree.canonical_epoch_snapshot();
                self.config
                    .node_config
                    .consensus_config()
                    .hardforks
                    .is_cascade_active_for_epoch(&epoch_snapshot)
            };
            if !cascade_active {
                return Err(TxIngressError::InvalidLedger(tx.ledger_id));
            }
        }
        // Fast-fail if we've recently seen this exact invalid payload (by signature fingerprint)
        {
            // Compute composite fingerprint: keccak(signature + prehash + id)
            // TODO: share the signature hash computed here with validate_signature
            let fingerprint = tx.fingerprint();
            if self
                .mempool_state
                .is_a_recent_invalid_fingerprint(&fingerprint)
                .await
            {
                return Err(TxIngressError::InvalidSignature(tx.signer));
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
        let anchor_height = self.validate_tx_anchor(tx).await?;
        let expiry_height = self.compute_expiry_height_from_anchor(anchor_height);

        // Validate and parse ledger type
        let ledger = self.parse_ledger(tx)?;

        Ok((ledger, expiry_height))
    }

    // Shared post-processing: insert into mempool, cache data_root with expiry,
    // process any pending chunks, and gossip the transaction.
    #[inline]
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_root = ?tx.data_root, expiry_height = expiry_height))]
    async fn postprocess_data_ingress(
        &self,
        tx: &DataTransactionHeader,
        expiry_height: u64,
    ) -> Result<(), TxIngressError> {
        self.mempool_state.insert_tx_and_mark_valid(tx).await?;
        self.cache_data_root_with_expiry(tx, expiry_height);
        // Notify the ChunkIngressService to process any pending chunks for this data root
        if let Err(e) = self
            .service_senders
            .chunk_ingress
            .send_traced(ChunkIngressMessage::ProcessPendingChunks(tx.data_root))
        {
            tracing::warn!(
                "Failed to send ProcessPendingChunks for data_root {:?}: {:?}",
                tx.data_root,
                e
            );
        }
        self.broadcast_tx_gossip(tx);
        metrics::record_data_tx_ingested();
        Ok(())
    }
    /// Check the mempool and mdbx for data transactions.
    /// Uses batch mempool lookup (single READ lock) then falls back to DB for missing txs.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.count = txs.len()))]
    pub async fn handle_get_data_tx_message(
        &self,
        txs: Vec<H256>,
    ) -> Vec<Option<DataTransactionHeader>> {
        // Batch mempool lookup: single READ lock for all txids
        let mempool_results = self
            .mempool_state
            .batch_valid_submit_ledger_tx_cloned(&txs)
            .await;

        let mut found_txs = Vec::with_capacity(txs.len());

        for (tx_id, mempool_result) in txs.iter().zip(mempool_results) {
            if let Some(tx_header) = mempool_result {
                trace!("Got tx {} from mempool", tx_id);
                found_txs.push(Some(tx_header));
                continue;
            }

            // Fall back to DB for txs not in mempool
            let db_result = self
                .irys_db
                .view(|read_tx| tx_header_by_txid(read_tx, tx_id))
                .map_err(|e| {
                    warn!("Failed to open DB read transaction: {}", e);
                    e
                })
                .ok()
                .and_then(|result| match result {
                    Ok(Some(tx_header)) => {
                        trace!("Got tx {} from DB", tx_id);
                        Some(tx_header)
                    }
                    Ok(None) => {
                        debug!("Tx {} not found in DB", tx_id);
                        None
                    }
                    Err(e) => {
                        warn!("DB error reading tx {}: {}", tx_id, e);
                        None
                    }
                });

            found_txs.push(db_result);
        }

        found_txs
    }

    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_root = ?tx.data_root))]
    pub async fn handle_data_tx_ingress_message_gossip(
        &self,
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
        if tx.promoted_height().is_some() {
            warn!(
                "Ingressed tx {:?} has promoted_height set to {:?}; preserving existing promotion state",
                tx.id,
                tx.promoted_height()
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
            DataLedger::OneYear | DataLedger::ThirtyDay => {
                // Term ledgers: validate perm_fee rejection and fee structure
                // (skip EMA pricing check since gossip may come from different fork)
                if tx
                    .perm_fee
                    .is_some_and(|f| f > irys_types::BoundedFee::zero())
                {
                    return Err(TxIngressError::FundMisalignment(
                        "Term-only ledger transactions must not have a perm_fee".to_string(),
                    ));
                }
                irys_types::transaction::fee_distribution::TermFeeCharges::new(
                    tx.term_fee,
                    &self.config.consensus,
                )
                .map_err(|e| {
                    TxIngressError::FundMisalignment(format!("Invalid term fee structure: {}", e))
                })?;
            }
            DataLedger::Submit => unreachable!("submit is not user-targetable"),
        }

        // Shared post-processing
        self.postprocess_data_ingress(&tx, expiry_height).await
    }

    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_root = ?tx.data_root))]
    pub async fn handle_data_tx_ingress_message_api(
        &self,
        mut tx: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx.id = ?tx.id,
            tx.data_root = ?tx.data_root,
            "Received data tx from API"
        );

        tx.set_promoted_height(None);

        // Shared pre-checks: duplicate detection, signature, anchor/expiry, ledger parsing
        let (ledger, expiry_height) = self.precheck_data_ingress_common(&tx).await?;

        // Validate fees against authoritative EMA pricing (API only)
        // This provides immediate feedback to API users if their transaction fees are insufficient
        self.validate_data_tx_ema_pricing(&tx)?;

        // Validate funding against canonical chain (API only)
        self.validate_data_tx_funding(&tx).await?;

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
            DataLedger::OneYear | DataLedger::ThirtyDay => {
                // Term-only ledgers must not carry a perm_fee
                if tx
                    .perm_fee
                    .is_some_and(|f| f > irys_types::BoundedFee::zero())
                {
                    return Err(TxIngressError::FundMisalignment(
                        "Term-only ledger transactions must not have a perm_fee".to_string(),
                    ));
                }
                // Validate term fee structure only
                TermFeeCharges::new(tx.term_fee, &self.config.node_config.consensus_config())
                    .map_err(|e| {
                        TxIngressError::FundMisalignment(format!(
                            "Invalid term fee structure: {}",
                            e
                        ))
                    })?;
            }
            DataLedger::Submit => unreachable!("submit is not user-targetable"),
        }

        // Shared post-processing
        self.postprocess_data_ingress(&tx, expiry_height).await
    }

    /// Validates that a data transaction has sufficient balance to cover its fees.
    /// Checks the balance against the canonical chain tip.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.signer = ?tx.signer))]
    async fn validate_data_tx_funding(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        // Fetch balance from canonical chain (None = canonical tip)
        let balance: U256 = self
            .reth_node_adapter
            .rpc
            .get_balance_irys_canonical_and_pending(tx.signer, None)
            .await
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
            metrics::record_data_tx_unfunded();
            return Err(TxIngressError::Unfunded(tx.id));
        }

        tracing::debug!(
            tx.id = %tx.id,
            account.balance = %balance,
            tx.required_balance = %required,
            "Funding validated for data tx"
        );

        Ok(())
    }

    /// Validates data transaction fees against the authoritative EMA pricing.
    /// Uses `ema_for_public_pricing()` which returns the stable price from 2 intervals ago.
    /// This is the price that users should use when calculating their transaction fees.
    ///
    /// Ensures that user-provided fees are >= minimum required based on current EMA pricing.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_size = tx.data_size))]
    fn validate_data_tx_ema_pricing(
        &self,
        tx: &DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        // Get the authoritative EMA pricing and latest block timestamp from canonical tip
        let (ema_snapshot, latest_block_timestamp_secs) = {
            let tree = self.block_tree_read_guard.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block_entry = canonical
                .last()
                .ok_or_else(|| TxIngressError::Other("Empty canonical chain".to_string()))?;
            let ema = tree
                .get_ema_snapshot(&last_block_entry.block_hash())
                .ok_or_else(|| TxIngressError::Other("EMA snapshot not found".to_string()))?;
            // Convert block timestamp from millis to seconds
            let timestamp_secs = last_block_entry.header().timestamp_secs();
            (ema, timestamp_secs)
        };

        let pricing_ema = ema_snapshot.ema_for_public_pricing();

        // Calculate expected fees using the authoritative EMA price
        let latest_height = self.get_latest_block_height()?;
        let next_block_height = latest_height + 1;

        let ledger = DataLedger::try_from(tx.ledger_id)
            .map_err(|_| TxIngressError::InvalidLedger(tx.ledger_id))?;

        let cascade = self.config.consensus.hardforks.cascade.as_ref();
        let epoch_length = match ledger {
            DataLedger::Publish | DataLedger::Submit => {
                self.config.consensus.epoch.submit_ledger_epoch_length
            }
            DataLedger::OneYear => cascade.map(|c| c.one_year_epoch_length).ok_or_else(|| {
                TxIngressError::FundMisalignment(
                    "Cascade hardfork not configured for OneYear ledger".to_string(),
                )
            })?,
            DataLedger::ThirtyDay => {
                cascade.map(|c| c.thirty_day_epoch_length).ok_or_else(|| {
                    TxIngressError::FundMisalignment(
                        "Cascade hardfork not configured for ThirtyDay ledger".to_string(),
                    )
                })?
            }
        };
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            next_block_height,
            self.config.consensus.epoch.num_blocks_in_epoch,
            epoch_length,
        );

        // Submit uses the full ingress proof replica count (part of perm pipeline).
        // OneYear/ThirtyDay have no ingress proofs — replica count is 1.
        let replica_count = match ledger {
            DataLedger::Publish | DataLedger::Submit => self
                .config
                .number_of_ingress_proofs_total_at(latest_block_timestamp_secs),
            DataLedger::OneYear | DataLedger::ThirtyDay => 1,
        };
        let expected_term_fee = calculate_term_fee(
            tx.data_size,
            epochs_for_storage,
            &self.config.consensus,
            replica_count,
            pricing_ema,
            latest_block_timestamp_secs,
        )
        .map_err(|e| {
            TxIngressError::FundMisalignment(format!("Failed to calculate term fee: {}", e))
        })?;

        // Validate term fee: user-provided must be >= expected
        if tx.term_fee < expected_term_fee {
            tracing::warn!(
                tx.id = %tx.id,
                tx.term_fee = %tx.term_fee,
                expected_term_fee = %expected_term_fee,
                pricing_ema = %pricing_ema.amount,
                "Data tx insufficient term_fee"
            );

            return Err(TxIngressError::FundMisalignment(format!(
                "Insufficient term fee: provided {}, required {}",
                tx.term_fee, expected_term_fee
            )));
        }

        // For Publish ledger, validate perm fee
        if let Ok(DataLedger::Publish) = DataLedger::try_from(tx.ledger_id) {
            let perm_fee = tx.perm_fee.ok_or_else(|| {
                TxIngressError::FundMisalignment("Publish tx missing perm_fee".to_string())
            })?;

            let expected_perm_fee = calculate_perm_fee_from_config(
                tx.data_size,
                &self.config.consensus,
                replica_count,
                pricing_ema,
                expected_term_fee,
                latest_block_timestamp_secs,
            )
            .map_err(|e| {
                TxIngressError::FundMisalignment(format!("Failed to calculate perm fee: {}", e))
            })?;

            // Validate perm fee: user-provided must be >= expected
            if perm_fee < expected_perm_fee.amount {
                tracing::warn!(
                    tx.id = %tx.id,
                    tx.perm_fee = %perm_fee,
                    expected_perm_fee = %expected_perm_fee.amount,
                    pricing_ema = %pricing_ema.amount,
                    "Data tx insufficient perm_fee"
                );

                return Err(TxIngressError::FundMisalignment(format!(
                    "Insufficient perm fee: provided {}, required {}",
                    perm_fee, expected_perm_fee.amount
                )));
            }
        }

        tracing::debug!(
            tx.id = %tx.id,
            pricing_ema = %pricing_ema.amount,
            term_fee = %tx.term_fee,
            "Data tx EMA pricing validated"
        );

        Ok(())
    }

    /// Computes the pre-confirmation expiry height given a resolved anchor height.
    fn compute_expiry_height_from_anchor(&self, anchor_height: u64) -> u64 {
        let anchor_expiry_depth = self.config.consensus.mempool.tx_anchor_expiry_depth as u64;
        anchor_height + anchor_expiry_depth
    }

    /// Parses the ledger id from the tx and maps errors to TxIngressError.
    fn parse_ledger(&self, tx: &DataTransactionHeader) -> Result<DataLedger, TxIngressError> {
        DataLedger::try_from(tx.ledger_id)
            .map_err(|_err| TxIngressError::InvalidLedger(tx.ledger_id))
    }

    /// Caches data_root with expiry, logging success/failure.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?tx.id, tx.data_root = ?tx.data_root, expiry_height = expiry_height))]
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

    /// Broadcasts the transaction over gossip, with error logging.
    fn broadcast_tx_gossip(&self, tx: &DataTransactionHeader) {
        let gossip_broadcast_message = GossipBroadcastMessageV2::from(tx.clone());
        if let Err(error) = self
            .service_senders
            .gossip_broadcast
            .send_traced(gossip_broadcast_message)
        {
            tracing::error!("Failed to send gossip data for tx {}: {:?}", tx.id, error);
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

        TermFeeCharges::new(actual_term_fee, &self.config.node_config.consensus_config()).map_err(
            |e| TxIngressError::FundMisalignment(format!("Invalid term fee structure: {}", e)),
        )?;

        // Get latest block's timestamp for hardfork params
        let latest_block_timestamp_secs = {
            let tree = self.block_tree_read_guard.read();
            let (canonical, _) = tree.get_canonical_chain();
            let last_block_entry = canonical
                .last()
                .ok_or_else(|| TxIngressError::Other("Empty canonical chain".to_string()))?;
            last_block_entry.header().timestamp_secs()
        };
        let number_of_ingress_proofs_total = self
            .config
            .number_of_ingress_proofs_total_at(latest_block_timestamp_secs);
        PublishFeeCharges::new(
            actual_perm_fee,
            actual_term_fee,
            &self.config.node_config.consensus_config(),
            number_of_ingress_proofs_total,
        )
        .map_err(|e| {
            TxIngressError::FundMisalignment(format!("Invalid perm fee structure: {}", e))
        })?;

        Ok(())
    }

    pub async fn get_all_storage_tx(&self) -> HashMap<IrysTransactionId, DataTransactionHeader> {
        let mut hash_map = HashMap::new();

        // first flat_map all the storage transactions
        // Get any DataTransaction from the valid storage txs
        self.mempool_state
            .all_valid_submit_ledgers_cloned()
            .await
            .into_values()
            .for_each(|tx| {
                hash_map.insert(tx.id, tx);
            });

        hash_map
    }

    /// checks mempool and mdbx
    pub async fn handle_data_tx_exists_message(
        &self,
        txid: H256,
    ) -> Result<TxKnownStatus, TxReadError> {
        let status = self.mempool_state.mempool_data_tx_status(&txid).await;
        if let Some(status) = status {
            return Ok(status);
        }

        match self.irys_db.view_eyre(|tx| tx_header_by_txid(tx, &txid)) {
            Ok(Some(_)) => Ok(TxKnownStatus::Migrated),
            Ok(None) => Ok(TxKnownStatus::Unknown),
            Err(_) => Err(TxReadError::DatabaseError),
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
        if optimistic.last().unwrap().0 != canonical_head_entry.block_hash() {
            debug!("Optimistic and Canonical have different heads");
        }

        let block_hash = canonical_head_entry.block_hash();
        let block_height = canonical_head_entry.height();

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
        let anchor_expiry_depth = self.config.consensus.mempool.tx_anchor_expiry_depth as u64;
        let min_anchor_height = block_height.saturating_sub(anchor_expiry_depth);

        // Start with all valid Submit ledger transactions - we'll filter out already-included ones
        let mut pending_valid_submit_ledger_tx =
            self.mempool_state.all_valid_submit_ledgers_cloned().await;

        // Walk backwards through the canonical chain, removing Submit transactions
        // that have already been included in recent blocks within the anchor expiry window
        while block.height >= min_anchor_height {
            let block_data_tx_ids = block.get_data_ledger_tx_ids();

            // Remove term ledger transactions that already exist in this historical block
            // This prevents double-inclusion and ensures we only return truly pending transactions
            for ledger in [
                DataLedger::Submit,
                DataLedger::OneYear,
                DataLedger::ThirtyDay,
            ] {
                if let Some(txids) = block_data_tx_ids.get(&ledger) {
                    for txid in txids.iter() {
                        pending_valid_submit_ledger_tx.remove(txid);
                    }
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
        pending_valid_submit_ledger_tx.into_values().collect()
    }
}
