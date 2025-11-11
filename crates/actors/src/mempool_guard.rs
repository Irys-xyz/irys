use crate::mempool_service::{AtomicMempoolState, MempoolState};
use irys_types::{CommitmentTransaction, IrysTransactionId};
use std::collections::HashMap;
use tokio::sync::RwLockReadGuard;

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

    /// Accessor method to get a read guard for the mempool state
    ///
    /// Note: This is an async operation because mempool uses tokio::sync::RwLock
    pub async fn read(&self) -> RwLockReadGuard<'_, MempoolState> {
        self.mempool_state.read().await
    }

    /// Get specific commitment transactions by their IDs from the mempool
    ///
    /// This searches both:
    /// - `valid_commitment_tx`: Validated commitment transactions organized by address
    /// - `pending_pledges`: Out-of-order pledge transactions waiting for dependencies
    ///
    /// Returns a HashMap containing only the requested transactions that were found.
    pub async fn get_commitment_txs(
        &self,
        commitment_tx_ids: &[IrysTransactionId],
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mempool_state_guard = self.mempool_state.read().await;

        let mut result = HashMap::with_capacity(commitment_tx_ids.len());

        // Optimize: Only search for the specific IDs we need instead of building a full hashmap
        for tx_id in commitment_tx_ids {
            // First, search in valid_commitment_tx
            let found = mempool_state_guard
                .valid_commitment_tx
                .values()
                .flat_map(|txs| txs.iter())
                .find(|tx| tx.id == *tx_id);

            if let Some(tx) = found {
                result.insert(*tx_id, tx.clone());
                continue;
            }

            // If not found, search in pending_pledges
            let found = mempool_state_guard
                .pending_pledges
                .iter()
                .flat_map(|(_, inner)| inner.iter())
                .find(|(id, _)| **id == *tx_id)
                .map(|(_, tx)| tx);

            if let Some(tx) = found {
                result.insert(*tx_id, tx.clone());
            }
        }

        result
    }
}
