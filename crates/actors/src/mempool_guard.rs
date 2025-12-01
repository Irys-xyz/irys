use crate::mempool_service::AtomicMempoolState;
use irys_types::{CommitmentTransaction, DataRoot, IrysTransactionId};
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

    /// Returns the number of pending chunks cached for a given data_root.
    pub async fn pending_chunk_count_for_data_root(&self, data_root: &DataRoot) -> usize {
        self.mempool_state
            .pending_chunk_count_for_data_root(data_root)
            .await
    }
}
