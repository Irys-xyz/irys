use crate::mempool_service::{Inner, TxIngressError, TxReadError, validate_commitment_transaction};
use irys_database::{commitment_tx_by_txid, db::IrysDatabaseExt as _};
use irys_domain::{CommitmentSnapshotStatus, HardforkConfigExt as _};
use irys_types::{
    CommitmentTransaction, CommitmentTypeV2, CommitmentValidationError, H256, IrysAddress,
    IrysTransactionCommon as _, IrysTransactionId, SendTraced as _, TxKnownStatus, UnixTimestamp,
    VersionDiscriminant as _,
};
// Bring RPC extension trait into scope for test contexts; `as _` avoids unused import warnings
use irys_types::gossip::v2::GossipBroadcastMessageV2;
use std::collections::HashMap;
use tracing::{debug, instrument, warn};

impl Inner {
    // Shared pre-checks for both API and Gossip commitment ingress paths.
    // Performs signature validation, whitelist check, mempool/db duplicate detection, and anchor validation.
    #[inline]
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
    async fn precheck_commitment_ingress_common(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        // Fast-fail if we've recently seen this exact invalid payload (by signature fingerprint)
        {
            // Compute composite fingerprint: keccak(signature + prehash + id)
            // TODO: share the signature hash computed here with validate_signature
            let fingerprint = commitment_tx.fingerprint();
            if self
                .mempool_state
                .is_a_recent_invalid_fingerprint(&fingerprint)
                .await
            {
                return Err(TxIngressError::InvalidSignature(commitment_tx.signer()));
            }
        }
        // Validate tx signature first to prevent ID poisoning
        if let Err(e) = self.validate_signature(commitment_tx).await {
            tracing::error!(
                "Signature validation for commitment_tx {:?} failed with error: {:?}",
                &commitment_tx,
                e
            );
            return Err(TxIngressError::InvalidSignature(commitment_tx.signer()));
        }

        // Validate commitment transaction version against hardfork rules
        let now = UnixTimestamp::now()
            .map_err(|e| TxIngressError::Other(format!("System time error: {}", e)))?;
        if !self
            .config
            .consensus
            .hardforks
            .is_commitment_version_valid(commitment_tx.version(), now)
        {
            let minimum = self
                .config
                .consensus
                .hardforks
                .minimum_commitment_version_at(now)
                .ok_or_else(|| {
                    TxIngressError::Other(
                        "Internal configuration error: minimum version not found".to_string(),
                    )
                })?;
            return Err(TxIngressError::InvalidVersion {
                version: commitment_tx.version(),
                minimum,
            });
        }

        // Validate commitment type against Borealis hardfork rules (epoch-aligned activation)
        if matches!(
            commitment_tx.commitment_type(),
            CommitmentTypeV2::UpdateRewardAddress { .. }
        ) {
            let epoch_snapshot = {
                let tree = self.block_tree_read_guard.read();
                tree.canonical_epoch_snapshot()
            };
            if !self
                .config
                .consensus
                .hardforks
                .is_update_reward_address_allowed_for_epoch(&epoch_snapshot)
            {
                return Err(TxIngressError::UpdateRewardAddressNotAllowed);
            }
        }

        // Check stake/pledge whitelist early - reject if address is not whitelisted
        self.check_commitment_whitelist(commitment_tx).await?;

        // Early out if we already know about this transaction (invalid/recent valid/valid_commitment_tx)
        if self
            .mempool_state
            .is_known_commitment_in_mempool(&commitment_tx.id(), commitment_tx.signer())
            .await
        {
            return Err(TxIngressError::Skipped);
        }

        // Early out if we already know about this transacti)on in index / database
        if self.is_known_commitment_in_db(&commitment_tx.id())? {
            return Err(TxIngressError::Skipped);
        }

        // Validate anchor (height is unused at this stage)
        self.validate_tx_anchor(commitment_tx).await?;

        Ok(())
    }

    // Shared post-validation processing for commitment transactions.
    // Computes commitment status and handles insert/cache/gossip accordingly.
    // The log_status_debug flag controls whether the status log is at debug (API) or trace (Gossip) level.
    // The warn_on_unstaked flag controls whether we emit a warning on Unstaked status (true for API only).
    #[inline]
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
    async fn process_commitment_after_prechecks(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<bool, TxIngressError> {
        let mut need_to_process_pending_pledges_and_then_gossip = false;
        // Check pending commitments and cached commitments and active commitments of the canonical chain
        let commitment_status = self.get_commitment_status(commitment_tx).await;
        debug!(
            "commitment tx {} status {:?}",
            &commitment_tx.id(),
            &commitment_status
        );

        match commitment_status {
            CommitmentSnapshotStatus::Unknown
            | CommitmentSnapshotStatus::Accepted
            | CommitmentSnapshotStatus::InvalidPledgeCount
            | CommitmentSnapshotStatus::Unowned
            | CommitmentSnapshotStatus::UnpledgePending
            | CommitmentSnapshotStatus::UnstakePending
            | CommitmentSnapshotStatus::HasActivePledges => {
                // Add to valid set and mark recent
                self.mempool_state
                    .insert_commitment_and_mark_valid(commitment_tx)
                    .await?;

                need_to_process_pending_pledges_and_then_gossip = true;
            }
            CommitmentSnapshotStatus::Unstaked => {
                warn!(
                    tx.id = ?commitment_tx.id(),
                    tx.commitment_status = ?commitment_status,
                    "commitment tx cached while address is unstaked"
                );
                // Cache pledge while address is unstaked
                self.mempool_state
                    .cache_unstaked_pledge(
                        commitment_tx,
                        self.config.node_config.mempool.max_pending_pledge_items,
                    )
                    .await;

                // Gossip the pledge even if signer is currently unstaked so other
                // nodes become aware of the pending pledge and can cache it as well.
                // This prevents loss if the first receiving node goes offline before
                // the signer stakes and triggers reprocessing.
                self.broadcast_commitment_gossip(commitment_tx);
            }
        }

        Ok(need_to_process_pending_pledges_and_then_gossip)
    }

    #[instrument(skip_all)]
    pub async fn handle_ingress_commitment_tx_message_gossip(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let need_to_process_pending_pledges_and_then_gossip =
            self.ingress_commitment_tx_gossip(&commitment_tx).await?;

        if need_to_process_pending_pledges_and_then_gossip {
            // Process any pending pledges for this newly staked address
            self.process_pending_pledges_for_new_stake(commitment_tx.signer())
                .await;
            // Gossip transaction
            self.broadcast_commitment_gossip(&commitment_tx);
        }

        Ok(())
    }

    #[instrument(skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
    async fn ingress_commitment_tx_gossip(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<bool, TxIngressError> {
        debug!(
            tx.id = ?commitment_tx.id(),
            tx.signer = ?commitment_tx.signer(),
            "Received commitment tx from Gossip"
        );

        // Common pre-checks shared with API path
        self.precheck_commitment_ingress_common(commitment_tx)
            .await?;

        // Gossip path: check only static fields from config (shape).
        // - Validate `fee` and `value` to reject clearly wrong Stake/Pledge/Unpledge/Unstake txs.
        // - Do not check account balance here. That is verified on API ingress
        //   and again during selection/block validation.
        if let Err(e) = commitment_tx.validate_fee(&self.config.consensus) {
            self.mempool_state
                .put_recent_invalid(commitment_tx.id())
                .await;

            return Err(e.into());
        }
        if let Err(e) = commitment_tx.validate_value(&self.config.consensus) {
            self.mempool_state
                .put_recent_invalid(commitment_tx.id())
                .await;

            return Err(e.into());
        }

        // Post-processing shared with API path (trace-level status, no warn on unstaked)
        self.process_commitment_after_prechecks(commitment_tx).await
    }

    #[instrument(skip_all)]
    pub async fn handle_ingress_commitment_tx_message_api(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx.id = ?commitment_tx.id(),
            tx.signer = ?commitment_tx.signer(),
            "Received commitment tx from API"
        );

        // Common pre-checks shared with Gossip path
        self.precheck_commitment_ingress_common(&commitment_tx)
            .await?;

        // API-only: fee/value/funding validations
        if let Err(e) = validate_commitment_transaction(
            &self.reth_node_adapter,
            &self.config.consensus,
            &commitment_tx,
            None,
        )
        .await
        {
            self.mempool_state
                .put_recent_invalid(commitment_tx.id())
                .await;

            return Err(e);
        }

        // Post-processing shared with Gossip path (debug-level status, warn on unstaked)
        let need_to_process_pending_pledges_and_then_gossip = self
            .process_commitment_after_prechecks(&commitment_tx)
            .await?;

        if need_to_process_pending_pledges_and_then_gossip {
            // Process any pending pledges for this newly staked address
            self.process_pending_pledges_for_new_stake(commitment_tx.signer())
                .await;
            // Gossip transaction
            self.broadcast_commitment_gossip(&commitment_tx);
        }

        Ok(())
    }

    /// Check stake/pledge whitelist; reject if address is not whitelisted.
    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id(), tx.signer = ?commitment_tx.signer()))]
    async fn check_commitment_whitelist(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        if !self
            .mempool_state
            .is_address_in_a_whitelist(&commitment_tx.signer())
            .await
        {
            warn!(
                "Commitment tx {} from address {} rejected: not in stake/pledge whitelist",
                commitment_tx.id(),
                commitment_tx.signer()
            );
            return Err(CommitmentValidationError::ForbiddenSigner.into());
        }
        Ok(())
    }

    /// Checks the database index for an existing commitment transaction by id.
    fn is_known_commitment_in_db(&self, tx_id: &H256) -> Result<bool, TxIngressError> {
        let known_in_db = self
            .irys_db
            .view_eyre(|dbtx| commitment_tx_by_txid(dbtx, tx_id))
            .map_err(|e| TxIngressError::DatabaseError(e.to_string()))?
            .is_some();
        Ok(known_in_db)
    }

    /// Processes any pending pledges for a newly staked address by re-ingesting them via gossip path.
    #[tracing::instrument(level = "trace", skip_all, fields(account.signer = ?signer))]
    async fn process_pending_pledges_for_new_stake(&self, signer: IrysAddress) {
        let pop = self
            .mempool_state
            .pop_pending_pledges_for_signer(&signer)
            .await;
        if let Some(pledges_lru) = pop {
            // Extract all pending pledges as a vector of owned transactions
            let pledges: Vec<_> = pledges_lru
                .into_iter()
                .map(|(_, pledge_tx)| pledge_tx)
                .collect();

            for pledge_tx in pledges {
                let tx_id = pledge_tx.id();
                if let Err(e) = self.ingress_commitment_tx_gossip(&pledge_tx).await {
                    warn!(
                        tx.id = ?tx_id,
                        tx.err = ?e,
                        "Failed to process pending pledge for newly staked address: {:?}",
                        e
                    );
                }
            }
        }
    }

    /// Broadcasts the commitment transaction over gossip.
    fn broadcast_commitment_gossip(&self, tx: &CommitmentTransaction) {
        self.service_senders
            .gossip_broadcast
            .send_traced(GossipBroadcastMessageV2::from(tx.clone()))
            .expect("Failed to send gossip data");
    }

    // checks recent_valid_tx, recent_invalid_tx, valid_commitment_tx, pending_pledges, and the database
    pub async fn handle_commitment_tx_exists_message(
        &self,
        commitment_tx_id: H256,
    ) -> Result<TxKnownStatus, TxReadError> {
        if let Some(status) = self
            .mempool_state
            .mempool_commitment_tx_status(&commitment_tx_id)
            .await
        {
            return Ok(status);
        }
        //now check the database
        match self
            .irys_db
            .view_eyre(|tx| commitment_tx_by_txid(tx, &commitment_tx_id))
        {
            Ok(Some(_)) => Ok(TxKnownStatus::Migrated),
            Ok(None) => Ok(TxKnownStatus::Unknown),
            Err(_) => Err(TxReadError::DatabaseError),
        }
    }

    /// read specified commitment txs from mempool
    #[instrument(level = "trace", skip_all, name = "get_commitment_tx", fields(tx.count = commitment_tx_ids.len()))]
    pub async fn handle_get_commitment_tx_message(
        &self,
        commitment_tx_ids: Vec<H256>,
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let all_commitment_transactions = self.mempool_state.all_commitment_transactions().await;

        debug!(
            "handle_get_commitment_transactions_message: {:?}",
            all_commitment_transactions
                .iter()
                .map(|x| x.0)
                .collect::<Vec<_>>()
        );

        // Attempt to locate and retain only the requested tx_ids
        let mut filtered_map = HashMap::with_capacity(commitment_tx_ids.len());
        for txid in commitment_tx_ids {
            if let Some(tx) = all_commitment_transactions.get(&txid) {
                filtered_map.insert(txid, tx.clone());
            }
        }

        // Return only the transactions matching the requested IDs
        filtered_map
    }

    #[tracing::instrument(level = "trace", skip_all, fields(tx.id = ?commitment_tx.id()))]
    pub async fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentSnapshotStatus {
        // Get the commitment snapshot for the current canonical chain
        let (commitment_snapshot, epoch_snapshot) = {
            let tree = self.block_tree_read_guard.read();
            (
                tree.canonical_commitment_snapshot(),
                tree.canonical_epoch_snapshot(),
            )
        };

        let cache_status =
            commitment_snapshot.get_commitment_status(commitment_tx, &epoch_snapshot);

        // Reject unsupported or invalid commitment types/targets
        match cache_status {
            CommitmentSnapshotStatus::Unknown | CommitmentSnapshotStatus::Accepted => {
                return cache_status;
            }
            CommitmentSnapshotStatus::UnstakePending
            | CommitmentSnapshotStatus::HasActivePledges
            | CommitmentSnapshotStatus::InvalidPledgeCount
            | CommitmentSnapshotStatus::Unowned
            | CommitmentSnapshotStatus::UnpledgePending => {
                warn!(
                    "Commitment rejected: {:?} id={} ",
                    cache_status,
                    commitment_tx.id()
                );
                return cache_status;
            }
            CommitmentSnapshotStatus::Unstaked => {
                // For unstaked addresses, check for pending stake transactions
                if self
                    .mempool_state
                    .is_there_a_pledge_for_unstaked_address(&commitment_tx.signer())
                    .await
                {
                    // Pending local stake makes this pledge/unpledge schedulable; mark as Unknown (fresh)
                    CommitmentSnapshotStatus::Unknown
                } else {
                    // No pending stakes found
                    warn!("Commitment is unstaked: {}", commitment_tx.id());
                    CommitmentSnapshotStatus::Unstaked
                }
            }
        }
    }
}
