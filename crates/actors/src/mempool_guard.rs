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
    ///
    /// Complexity: O(n + m) where n is the number of requested IDs and m is the total
    /// number of transactions in the mempool.
    #[must_use]
    pub async fn get_commitment_txs(
        &self,
        commitment_tx_ids: &[IrysTransactionId],
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        const PRESUMED_PLEDGES_PER_ACCOUNT: usize = 4;
        let mempool_state_guard = self.mempool_state.read().await;

        // Build lookup map of ALL transactions in mempool - O(m)
        let mut all_txs = HashMap::with_capacity(
            mempool_state_guard.valid_commitment_tx.len()
                + mempool_state_guard.pending_pledges.len() * PRESUMED_PLEDGES_PER_ACCOUNT,
        );

        // Collect from valid_commitment_tx
        for txs in mempool_state_guard.valid_commitment_tx.values() {
            for tx in txs {
                all_txs.insert(tx.id, tx);
            }
        }

        // Collect from pending_pledges
        for (_, inner_cache) in mempool_state_guard.pending_pledges.iter() {
            for (id, tx) in inner_cache.iter() {
                all_txs.insert(*id, tx);
            }
        }

        // Lookup requested transactions - O(n)
        commitment_tx_ids
            .iter()
            .filter_map(|tx_id| all_txs.get(tx_id).map(|tx| (*tx_id, (*tx).clone())))
            .collect()
    }
}
