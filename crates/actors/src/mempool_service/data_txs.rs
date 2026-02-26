use crate::chunk_ingress_service::ChunkIngressMessage;
use crate::mempool_service::validate_tx_signature;
use crate::mempool_service::TxIngressError;
use crate::mempool_service::{Inner, TxReadError};
use crate::metrics;
use eyre::eyre;
use irys_database::{db::IrysDatabaseExt as _, tables::CachedDataRoots, tx_header_by_txid};
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_types::TxKnownStatus;
use irys_types::storage_pricing::{calculate_perm_fee_from_config, calculate_term_fee};
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    DataLedger, DataTransactionHeader, H256, IrysTransactionCommon as _, IrysTransactionId,
    SendTraced as _, U256,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
};
use reth_db::transaction::DbTxMut as _;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

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
        // Fast-fail if this tx targets an unsupported term ledger
        match DataLedger::try_from(tx.ledger_id) {
            Ok(DataLedger::Publish | DataLedger::Submit) => {
                // Valid ledgers - continue
            }
            _ => return Err(TxIngressError::InvalidLedger(tx.ledger_id)),
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
        if let Err(e) = validate_tx_signature(tx) {
            self.mempool_state
                .mark_fingerprint_as_invalid(tx.fingerprint())
                .await;
            return Err(e);
        }

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
            DataLedger::OneYear | DataLedger::ThirtyDay | DataLedger::Submit => {
                // Term ledgers and direct Submit targeting are not yet supported
                return Err(TxIngressError::InvalidLedger(ledger as u32));
            }
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
            DataLedger::OneYear | DataLedger::ThirtyDay | DataLedger::Submit => {
                // Term ledgers and direct Submit targeting are not yet supported
                return Err(TxIngressError::InvalidLedger(ledger as u32));
            }
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
        let latest_height =
            crate::anchor_validation::get_latest_block_height(&self.block_tree_read_guard)?;
        let next_block_height = latest_height + 1;
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            next_block_height,
            self.config.consensus.epoch.num_blocks_in_epoch,
            self.config.consensus.epoch.submit_ledger_epoch_length,
        );

        // Use latest block's timestamp for hardfork params
        let number_of_ingress_proofs_total = self
            .config
            .number_of_ingress_proofs_total_at(latest_block_timestamp_secs);
        let expected_term_fee = calculate_term_fee(
            tx.data_size,
            epochs_for_storage,
            &self.config.consensus,
            number_of_ingress_proofs_total,
            pricing_ema,
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
                number_of_ingress_proofs_total,
                pricing_ema,
                expected_term_fee,
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
}
