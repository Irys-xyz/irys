use crate::mempool_service::AtomicMempoolState;
use irys_types::{CommitmentTransaction, DataRoot, DataTransactionHeader, IrysTransactionId};
use std::collections::HashMap;

/// Wraps the internal `Arc<RwLock<_>>` to provide readonly access to mempool state
#[derive(Debug, Clone)]
pub struct MempoolReadGuard {
    mempool_state: AtomicMempoolState,
}

impl MempoolReadGuard {
    /// Creates a new `MempoolReadGuard` for readonly access to mempool state
    pub const fn new(mempool_state: AtomicMempoolState) -> Self {
        Self { mempool_state }
    }

    /// Creates an empty `MempoolReadGuard` for testing purposes
    #[cfg(any(test, feature = "test-utils"))]
    pub fn stub() -> Self {
        use crate::mempool_service::create_state;
        use irys_types::MempoolConfig;

        let config = MempoolConfig::testing();
        let state = create_state(&config, &[]);
        Self::new(AtomicMempoolState::new(state))
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
        self.mempool_state
            .get_commitment_txs(commitment_tx_ids)
            .await
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
        self.mempool_state.get_data_txs(data_tx_ids).await
    }

    /// Returns the number of pending chunks cached for a given data_root.
    pub async fn pending_chunk_count_for_data_root(&self, data_root: &DataRoot) -> usize {
        self.mempool_state
            .pending_chunk_count_for_data_root(data_root)
            .await
    }

    /// Check if a transaction ID is in the recent valid transactions cache
    pub fn is_recent_valid_tx_blocking(&self, tx_id: &IrysTransactionId) -> bool {
        let state = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.mempool_state.read())
        });
        state.recent_valid_tx.contains(tx_id)
    }

    /// Get transaction metadata from mempool (returns None if not found or no metadata)
    pub fn get_tx_metadata_blocking(&self, tx_id: &IrysTransactionId) -> Option<irys_types::TransactionMetadata> {
        let state = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.mempool_state.read())
        });
        
        // Check data transactions
        if let Some(wrapped_tx) = state.valid_submit_ledger_tx.get(tx_id) {
            return Some(wrapped_tx.metadata.clone());
        }
        
        // Check commitment transactions
        for txs in state.valid_commitment_tx.values() {
            if let Some(wrapped_tx) = txs.iter().find(|w| w.id() == *tx_id) {
                return Some(wrapped_tx.metadata.clone());
            }
        }
        
        None
    }
}
