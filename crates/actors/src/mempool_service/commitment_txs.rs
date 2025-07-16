use crate::mempool_service::{Inner, MempoolServiceMessage, TxIngressError, TxReadError};
use base58::ToBase58 as _;
use irys_database::{commitment_tx_by_txid, db::IrysDatabaseExt as _};
use irys_domain::CommitmentSnapshotStatus;
use irys_primitives::CommitmentType;
use irys_types::{Address, CommitmentTransaction, GossipBroadcastMessage, IrysTransactionId, H256};
use lru::LruCache;
use std::{collections::HashMap, num::NonZeroUsize};
use tracing::{debug, warn};

impl Inner {
    pub async fn handle_ingress_commitment_tx_message(
        &mut self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        debug!(
            "received commitment tx {:?}",
            &commitment_tx.id.0.to_base58()
        );

        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().await;

        // Early out if we already know about this transaction in mempool (invalid)
        if mempool_state_guard.invalid_tx.contains(&commitment_tx.id) {
            return Err(TxIngressError::Skipped);
        }

        // Early out if we already know about this transaction in mempool (recent valid)
        if mempool_state_guard
            .recent_valid_tx
            .contains(&commitment_tx.id)
        {
            return Err(TxIngressError::Skipped);
        }

        //  Early out if the transaction already exists in valid_commitment_tx
        if mempool_state_guard
            .valid_commitment_tx
            .get(&commitment_tx.signer)
            .is_some_and(|txs| txs.iter().any(|c| c.id == commitment_tx.id))
        {
            return Err(TxIngressError::Skipped);
        }

        drop(mempool_state_guard);

        // Early out if we already know about this transaction in index / database
        if self
            .irys_db
            .view_eyre(|dbtx| commitment_tx_by_txid(dbtx, &commitment_tx.id))
            .map_err(|_| TxIngressError::DatabaseError)?
            .is_some()
        {
            return Err(TxIngressError::Skipped);
        }

        // Early out if this is a stake tx from an address that is already
        // staked for the current epoch or the address has another pending stake tx
        if commitment_tx.commitment_type == CommitmentType::Stake {
            let mempool_state_guard = mempool_state.read().await;
            // check mempool commitment stake txs
            if mempool_state_guard
                .valid_commitment_tx
                .get(&commitment_tx.signer)
                .is_some_and(|txs| {
                    txs.iter()
                        .any(|c| c.commitment_type == CommitmentType::Stake)
                })
            {
                return Err(TxIngressError::Skipped);
            }

            // check commitments in epochs
            let commitment_snapshot = self
                .block_tree_read_guard
                .read()
                .canonical_commitment_snapshot();

            let is_staked = self
                .block_tree_read_guard
                .read()
                .canonical_epoch_snapshot()
                .is_staked(commitment_tx.signer);

            if commitment_snapshot.get_commitment_status(&commitment_tx, is_staked)
                == CommitmentSnapshotStatus::Accepted
            {
                Err(TxIngressError::Skipped)?
            }
        }

        // Validate the tx anchor
        if let Err(e) = self
            .validate_anchor(&commitment_tx.id, &commitment_tx.anchor)
            .await
        {
            tracing::warn!(
                "Anchor {:?} for tx {:?} failure with error: {:?}",
                &commitment_tx.anchor,
                commitment_tx.id,
                e
            );
            return Err(TxIngressError::InvalidAnchor);
        }

        // Check pending commitments and cached commitments and active commitments of the canonical chain
        let commitment_status = self.get_commitment_status(&commitment_tx).await;
        if commitment_status == CommitmentSnapshotStatus::Accepted {
            // Validate tx signature
            if let Err(e) = self.validate_signature(&commitment_tx).await {
                tracing::error!(
                    "Signature validation for commitment_tx {:?} failed with error: {:?}",
                    &commitment_tx,
                    e
                );
                return Err(TxIngressError::InvalidSignature);
            }

            let mut mempool_state_guard = mempool_state.write().await;
            // Add the commitment tx to the valid tx list to be included in the next block
            mempool_state_guard
                .valid_commitment_tx
                .entry(commitment_tx.signer)
                .or_default()
                .push(commitment_tx.clone());

            mempool_state_guard.recent_valid_tx.insert(commitment_tx.id);

            // Process any pending pledges for this newly staked address
            // ------------------------------------------------------
            // When a stake transaction is accepted, we can now process any pledge
            // transactions from the same address that arrived earlier but were
            // waiting for the stake. This effectively resolves the dependency
            // order for address-based validation.
            let pop = mempool_state_guard
                .pending_pledges
                .pop(&commitment_tx.signer);
            drop(mempool_state_guard);
            if let Some(pledges_lru) = pop {
                // Extract all pending pledges as a vector of owned transactions
                let pledges: Vec<_> = pledges_lru
                    .into_iter()
                    .map(|(_, pledge_tx)| pledge_tx)
                    .collect();

                for pledge_tx in pledges {
                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    // todo switch _ to actually handle the result
                    let _ = self
                        .handle_message(MempoolServiceMessage::IngestCommitmentTx(
                            pledge_tx, oneshot_tx,
                        ))
                        .await;

                    let _ = oneshot_rx
                        .await
                        .expect("to process pending pledge for newly staked address");
                }
            }

            // Gossip transaction
            self.service_senders
                .gossip_broadcast
                .send(GossipBroadcastMessage::from(commitment_tx.clone()))
                .expect("Failed to send gossip data");
        } else if commitment_status == CommitmentSnapshotStatus::Unstaked {
            // For unstaked pledges, we cache them in a 2-level LRU structure:
            // Level 1: Keyed by signer address (allows tracking multiple addresses)
            // Level 2: Keyed by transaction ID (allows tracking multiple pledge tx per address)

            let mut mempool_state_guard = mempool_state.write().await;
            if let Some(pledges_cache) = mempool_state_guard
                .pending_pledges
                .get_mut(&commitment_tx.signer)
            {
                // Address already exists in cache - add this pledge transaction to its lru cache
                pledges_cache.put(commitment_tx.id, commitment_tx.clone());
            } else {
                // First pledge from this address - create a new nested lru cache
                let max_pending_pledge_items =
                    self.config.consensus.mempool.max_pending_pledge_items;
                let mut new_address_cache =
                    LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap());

                // Add the pledge transaction to the new lru cache for the address
                new_address_cache.put(commitment_tx.id, commitment_tx.clone());

                // Add the address cache to the primary lru cache
                mempool_state_guard
                    .pending_pledges
                    .put(commitment_tx.signer, new_address_cache);
            }
            drop(mempool_state_guard)
        } else {
            return Err(TxIngressError::Skipped);
        }
        Ok(())
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
    pub async fn handle_get_commitment_tx_message(
        &self,
        commitment_tx_ids: Vec<H256>,
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

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

        mempool_state_guard.recent_valid_tx.remove(txid);

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

    pub async fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentSnapshotStatus {
        // Get the commitment snapshot for the current canonical chain
        let commitment_snapshot = self
            .block_tree_read_guard
            .read()
            .canonical_commitment_snapshot();

        let epoch_snapshot = self.block_tree_read_guard.read().canonical_epoch_snapshot();

        let is_staked = epoch_snapshot.is_staked(commitment_tx.signer);

        let cache_status = commitment_snapshot.get_commitment_status(commitment_tx, is_staked);

        // Reject unsupported commitment types
        if matches!(cache_status, CommitmentSnapshotStatus::Unsupported) {
            warn!(
                "Commitment is unsupported: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentSnapshotStatus::Unsupported;
        }

        // For unstaked addresses, check for pending stake transactions
        if matches!(cache_status, CommitmentSnapshotStatus::Unstaked) {
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
                    return CommitmentSnapshotStatus::Accepted;
                }
            }

            // No pending stakes found
            warn!(
                "Pledge Commitment is unstaked: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentSnapshotStatus::Unstaked;
        }

        // All other cases are valid
        CommitmentSnapshotStatus::Accepted
    }
}
