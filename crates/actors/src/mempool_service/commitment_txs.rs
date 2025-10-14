use crate::mempool_service::{
    validate_commitment_transaction, Inner, MempoolServiceMessage, TxIngressError, TxReadError,
};
use irys_database::{commitment_tx_by_txid, db::IrysDatabaseExt as _};
use irys_domain::CommitmentSnapshotStatus;
use irys_primitives::CommitmentType;
use irys_types::{
    Address, CommitmentTransaction, CommitmentValidationError, GossipBroadcastMessage,
    IrysTransactionId, Signable as _, H256,
};
use lru::LruCache;
// Bring RPC extension trait into scope for test contexts; `as _` avoids unused import warnings
use reth::revm::primitives::alloy_primitives;
use std::{collections::HashMap, num::NonZeroUsize};
use tracing::{debug, instrument, warn};

impl Inner {
    // Shared pre-checks for both API and Gossip commitment ingress paths.
    // Performs signature validation, whitelist check, mempool/db duplicate detection, and anchor validation.
    #[inline]
    async fn precheck_commitment_ingress_common(
        &mut self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        // Fast-fail if we've recently seen this exact invalid payload (by signature fingerprint)
        {
            // Compute composite fingerprint: keccak(signature + prehash)
            let prehash = commitment_tx.signature_hash();
            let mut buf = Vec::with_capacity(65 + 32);
            buf.extend_from_slice(&commitment_tx.signature.as_bytes());
            buf.extend_from_slice(&prehash);
            let fingerprint = H256::from(alloy_primitives::keccak256(&buf).0);
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
        // Validate tx signature first to prevent ID poisoning
        if let Err(e) = self.validate_signature(commitment_tx).await {
            tracing::error!(
                "Signature validation for commitment_tx {:?} failed with error: {:?}",
                &commitment_tx,
                e
            );
            return Err(TxIngressError::InvalidSignature);
        }

        // Check stake/pledge whitelist early - reject if address is not whitelisted
        self.check_commitment_whitelist(commitment_tx)?;

        // Early out if we already know about this transaction (invalid/recent valid/valid_commitment_tx)
        if self
            .is_known_commitment_in_mempool(&commitment_tx.id, commitment_tx.signer)
            .await
        {
            return Err(TxIngressError::Skipped);
        }

        // Early out if we already know about this transaction in index / database
        if self.is_known_commitment_in_db(&commitment_tx.id)? {
            return Err(TxIngressError::Skipped);
        }

        // Validate anchor (height is unused at this stage)
        self.validate_anchor(commitment_tx).await?;

        Ok(())
    }

    // Shared post-validation processing for commitment transactions.
    // Computes commitment status and handles insert/cache/gossip accordingly.
    // The log_status_debug flag controls whether the status log is at debug (API) or trace (Gossip) level.
    // The warn_on_unstaked flag controls whether we emit a warning on Unstaked status (true for API only).
    #[inline]
    async fn process_commitment_after_prechecks(
        &mut self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        // Check pending commitments and cached commitments and active commitments of the canonical chain
        let commitment_status = self.get_commitment_status(commitment_tx).await;
        debug!(
            "commitment tx {} status {:?}",
            &commitment_tx.id, &commitment_status
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
                self.insert_commitment_and_mark_valid(commitment_tx).await;

                // Process any pending pledges for this newly staked address
                self.process_pending_pledges_for_new_stake(commitment_tx.signer)
                    .await;

                // Gossip transaction
                self.broadcast_commitment_gossip(commitment_tx);
            }
            CommitmentSnapshotStatus::Unstaked => {
                warn!(
                    tx = ?commitment_tx.id,
                    status = ?commitment_status,
                    "commitment tx cached while address is unstaked"
                );
                // Cache pledge while address is unstaked
                self.cache_unstaked_pledge(commitment_tx).await;

                // Gossip the pledge even if signer is currently unstaked so other
                // nodes become aware of the pending pledge and can cache it as well.
                // This prevents loss if the first receiving node goes offline before
                // the signer stakes and triggers reprocessing.
                self.broadcast_commitment_gossip(commitment_tx);
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn handle_ingress_commitment_tx_message_gossip(
        &mut self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx_id = ?commitment_tx.id,
            signer = ?commitment_tx.signer,
            "Received commitment tx from Gossip"
        );

        // Common pre-checks shared with API path
        self.precheck_commitment_ingress_common(&commitment_tx)
            .await?;

        // Validate economic constraints only for API-submitted transactions
        // Gossip-submitted transactions skip these to avoid rejecting txs from other forks
        // while still being able to process them when building blocks.
        // Gossip path: skip fee/value/funding checks

        // Post-processing shared with API path (trace-level status, no warn on unstaked)
        self.process_commitment_after_prechecks(&commitment_tx)
            .await
    }

    #[instrument(skip_all)]
    pub async fn handle_ingress_commitment_tx_message_api(
        &mut self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        debug!(
            tx_id = ?commitment_tx.id,
            signer = ?commitment_tx.signer,
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
        ) {
            let mut guard = self.mempool_state.write().await;
            guard.recent_invalid_tx.put(commitment_tx.id, ());
            drop(guard);

            return Err(e);
        }

        // Post-processing shared with Gossip path (debug-level status, warn on unstaked)
        self.process_commitment_after_prechecks(&commitment_tx)
            .await
    }

    /// Check stake/pledge whitelist; reject if address is not whitelisted.
    fn check_commitment_whitelist(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let whitelist = &self.stake_and_pledge_whitelist;
        if !whitelist.is_empty() && !whitelist.contains(&commitment_tx.signer) {
            warn!(
                "Commitment tx {} from address {} rejected: not in stake/pledge whitelist",
                commitment_tx.id, commitment_tx.signer
            );
            return Err(CommitmentValidationError::ForbiddenSigner.into());
        }
        Ok(())
    }

    /// Returns true if the commitment tx is already known in the mempool caches/maps.
    async fn is_known_commitment_in_mempool(&self, tx_id: &H256, signer: Address) -> bool {
        let guard = self.mempool_state.read().await;
        // Only treat recent valid entries as known. Invalid must not block legitimate re-ingress.
        if guard.recent_valid_tx.contains(tx_id) {
            return true;
        }
        if guard
            .valid_commitment_tx
            .get(&signer)
            .is_some_and(|txs| txs.iter().any(|c| c.id == *tx_id))
        {
            return true;
        }
        false
    }

    /// Checks the database index for an existing commitment transaction by id.
    fn is_known_commitment_in_db(&self, tx_id: &H256) -> Result<bool, TxIngressError> {
        let known_in_db = self
            .irys_db
            .view_eyre(|dbtx| commitment_tx_by_txid(dbtx, tx_id))
            .map_err(|_| TxIngressError::DatabaseError)?
            .is_some();
        Ok(known_in_db)
    }

    /// Inserts a commitment into the mempool valid map and marks it as recently valid.
    async fn insert_commitment_and_mark_valid(&mut self, tx: &CommitmentTransaction) {
        let mut guard = self.mempool_state.write().await;
        guard
            .valid_commitment_tx
            .entry(tx.signer)
            .or_default()
            .push(tx.clone());
        guard.recent_valid_tx.put(tx.id, ());
    }

    /// Processes any pending pledges for a newly staked address by re-ingesting them via gossip path.
    async fn process_pending_pledges_for_new_stake(&mut self, signer: Address) {
        let mut guard = self.mempool_state.write().await;
        let pop = guard.pending_pledges.pop(&signer);
        drop(guard);
        if let Some(pledges_lru) = pop {
            // Extract all pending pledges as a vector of owned transactions
            let pledges: Vec<_> = pledges_lru
                .into_iter()
                .map(|(_, pledge_tx)| pledge_tx)
                .collect();

            for pledge_tx in pledges {
                let tx_id = pledge_tx.id;
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                if let Err(e) = self
                    .handle_message(MempoolServiceMessage::IngestCommitmentTxFromGossip(
                        pledge_tx, oneshot_tx,
                    ))
                    .await
                {
                    warn!(
                        tx = ?tx_id,
                        error = ?e,
                        "Failed to process pending pledge for newly staked address"
                    );
                }

                if let Err(e) = oneshot_rx
                    .await
                    .expect("to process pending pledge for newly staked address")
                {
                    warn!(
                        "Failed to process pending pledge for newly staked address: {:?}",
                        e
                    );
                }
            }
        }
    }

    /// Caches an unstaked pledge in the two-level LRU structure.
    async fn cache_unstaked_pledge(&mut self, tx: &CommitmentTransaction) {
        let mut guard = self.mempool_state.write().await;
        if let Some(pledges_cache) = guard.pending_pledges.get_mut(&tx.signer) {
            // Address already exists in cache - add this pledge transaction to its lru cache
            pledges_cache.put(tx.id, tx.clone());
        } else {
            // First pledge from this address - create a new nested lru cache
            let max_pending_pledge_items = self.config.mempool.max_pending_pledge_items;
            let mut new_address_cache =
                LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap());

            // Add the pledge transaction to the new lru cache for the address
            new_address_cache.put(tx.id, tx.clone());

            // Add the address cache to the primary lru cache
            guard.pending_pledges.put(tx.signer, new_address_cache);
        }
    }

    /// Broadcasts the commitment transaction over gossip.
    fn broadcast_commitment_gossip(&self, tx: &CommitmentTransaction) {
        self.service_senders
            .gossip_broadcast
            .send(GossipBroadcastMessage::from(tx.clone()))
            .expect("Failed to send gossip data");
    }

    /// checks only the mempool
    pub async fn handle_commitment_tx_exists_message(
        &self,
        commitment_tx_id: H256,
    ) -> Result<bool, TxReadError> {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().await;

        Ok(mempool_state_guard
            .valid_commitment_tx
            .values()
            .flatten()
            .any(|tx| tx.id == commitment_tx_id))
    }

    /// read specified commitment txs from mempool
    #[instrument(skip_all, name = "get_commitment_tx")]
    pub async fn handle_get_commitment_tx_message(
        &self,
        commitment_tx_ids: Vec<H256>,
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state_guard = self.mempool_state.read().await;

        // TODO: what the heck is this, this needs to be optimised at least a little bit

        // Get any CommitmentTransactions from the valid commitments Map
        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
            });

        // Get any CommitmentTransactions from the pending commitments LRU cache
        mempool_state_guard
            .pending_pledges
            .iter()
            .flat_map(|(_, inner)| inner.iter())
            .for_each(|(tx_id, tx)| {
                hash_map.insert(*tx_id, tx.clone());
            });

        debug!(
            "handle_get_commitment_transactions_message: {:?}",
            hash_map.iter().map(|x| x.0).collect::<Vec<_>>()
        );

        // Attempt to locate and retain only the requested tx_ids
        let mut filtered_map = HashMap::with_capacity(commitment_tx_ids.len());
        for txid in commitment_tx_ids {
            if let Some(tx) = hash_map.get(&txid) {
                filtered_map.insert(txid, tx.clone());
            }
        }

        // Return only the transactions matching the requested IDs
        filtered_map
    }

    /// should really only be called by persist_mempool_to_disk, all other scenarios need a more
    /// subtle filtering of commitment state, recently confirmed? pending? valid? etc.
    pub async fn get_all_commitment_tx(&self) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // Get any CommitmentTransactions from the valid commitments
        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
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

    /// Removes a commitment transaction with the specified transaction ID from the valid_commitment_tx map
    /// Returns true if the transaction was found and removed, false otherwise
    pub async fn remove_commitment_tx(&mut self, txid: &H256) -> bool {
        let mut found = false;

        let mempool_state = &self.mempool_state;
        let mut mempool_state_guard = mempool_state.write().await;

        mempool_state_guard.recent_valid_tx.pop(txid);

        // Create a vector of addresses to update to avoid borrowing issues
        let addresses_to_check: Vec<Address> = mempool_state_guard
            .valid_commitment_tx
            .keys()
            .copied()
            .collect();

        for address in addresses_to_check {
            if let Some(transactions) = mempool_state_guard.valid_commitment_tx.get_mut(&address) {
                // Find the index of the transaction to remove
                if let Some(index) = transactions.iter().position(|tx| tx.id == *txid) {
                    // Remove the transaction
                    transactions.remove(index);
                    found = true;

                    // If the vector is now empty, remove the entry
                    if transactions.is_empty() {
                        mempool_state_guard.valid_commitment_tx.remove(&address);
                    }

                    // Exit early once we've found and removed the transaction
                    break;
                }
            }
        }

        drop(mempool_state_guard);

        found
    }

    #[tracing::instrument(skip_all, fields(tx_id = ?commitment_tx.id))]
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
                return cache_status
            }
            CommitmentSnapshotStatus::UnstakePending
            | CommitmentSnapshotStatus::HasActivePledges
            | CommitmentSnapshotStatus::InvalidPledgeCount
            | CommitmentSnapshotStatus::Unowned
            | CommitmentSnapshotStatus::UnpledgePending => {
                warn!(
                    "Commitment rejected: {:?} id={} ",
                    cache_status, commitment_tx.id
                );
                return cache_status;
            }
            CommitmentSnapshotStatus::Unstaked => {
                // For unstaked addresses, check for pending stake transactions
                let mempool_state_guard = self.mempool_state.read().await;
                // Get pending transactions for this address
                if let Some(pending) = mempool_state_guard
                    .valid_commitment_tx
                    .get(&commitment_tx.signer)
                {
                    // Check if there's at least one pending stake transaction
                    if pending
                        .iter()
                        .any(|c| c.commitment_type == CommitmentType::Stake)
                    {
                        // Pending local stake makes this pledge/unpledge schedulable; mark as Unknown (fresh)
                        return CommitmentSnapshotStatus::Unknown;
                    }
                }

                // No pending stakes found
                warn!("Commitment is unstaked: {}", commitment_tx.id);
                return CommitmentSnapshotStatus::Unstaked;
            }
        }
    }
}
