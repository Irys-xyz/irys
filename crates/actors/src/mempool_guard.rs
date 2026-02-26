use crate::mempool_service::AtomicMempoolState;
use irys_database::{db::IrysDatabaseExt as _, tx_header_by_txid};
use irys_types::{
    CommitmentTransaction, CommitmentTransactionMetadata, DataTransactionHeader,
    DataTransactionMetadata, IrysTransactionId, app_state::DatabaseProvider,
};
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

    /// Returns a reference to the underlying `AtomicMempoolState`.
    /// This is primarily used by the block producer to construct a `TxSelectionContext`
    /// for direct tx selection without going through the mempool message queue.
    pub fn atomic_state(&self) -> &AtomicMempoolState {
        &self.mempool_state
    }

    /// Check if a transaction ID is in the recent valid transactions cache.
    pub async fn is_recent_valid_tx(&self, tx_id: &IrysTransactionId) -> bool {
        self.mempool_state.is_recent_valid_tx(tx_id).await
    }

    /// Get transaction metadata from mempool (returns None if not found or no metadata).
    /// This checks both commitment and data transaction metadata.
    pub async fn get_tx_metadata(&self, tx_id: &IrysTransactionId) -> Option<TxMetadata> {
        self.mempool_state.get_tx_metadata(tx_id).await
    }
}

/// Enum to represent either commitment or data metadata
#[derive(Debug, Clone)]
pub enum TxMetadata {
    Commitment(CommitmentTransactionMetadata),
    Data(DataTransactionMetadata),
}

impl TxMetadata {
    pub fn included_height(&self) -> Option<u64> {
        match self {
            Self::Commitment(m) => m.included_height,
            Self::Data(m) => m.included_height,
        }
    }

    pub fn promoted_height(&self) -> Option<u64> {
        match self {
            Self::Commitment(_) => None,
            Self::Data(m) => m.promoted_height,
        }
    }
}

/// Best-effort fetch: mempool first, DB fallback for missing txs.
/// Preserves the semantics of the old `GetDataTxs` message handler.
/// Kept as a free function to avoid mixing DB concerns into `MempoolReadGuard`.
pub async fn get_data_txs_best_effort(
    guard: &MempoolReadGuard,
    tx_ids: &[IrysTransactionId],
    db: &DatabaseProvider,
) -> Vec<Option<DataTransactionHeader>> {
    let mempool_results = guard
        .atomic_state()
        .batch_valid_submit_ledger_tx_cloned(tx_ids)
        .await;

    tx_ids
        .iter()
        .zip(mempool_results)
        .map(|(tx_id, mempool_result)| {
            mempool_result.or_else(|| {
                db.view_eyre(|read_tx| {
                    tx_header_by_txid(read_tx, tx_id).map_err(|e| eyre::eyre!("{:?}", e))
                })
                .ok()
                .flatten()
            })
        })
        .collect()
}
