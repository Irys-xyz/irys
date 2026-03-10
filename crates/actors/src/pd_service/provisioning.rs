use super::cache::ChunkKey;
use reth::revm::primitives::B256;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Per-transaction provisioning state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningState {
    /// Fetching chunks from local storage (+ future gossip).
    Provisioning,
    /// All chunks available in cache.
    Ready,
    /// Some chunks unavailable (P2P not yet implemented).
    PartiallyReady { found: usize, total: usize },
}

/// Tracks the provisioning lifecycle for a single PD transaction.
pub struct TxProvisioningState {
    pub tx_hash: B256,
    /// All chunks this transaction needs.
    pub required_chunks: HashSet<ChunkKey>,
    /// Chunks that could not be found locally.
    pub missing_chunks: HashSet<ChunkKey>,
    /// Current lifecycle state.
    pub state: ProvisioningState,
    /// When provisioning started.
    pub started_at: Instant,
}

/// Manages per-transaction provisioning state.
pub struct ProvisioningTracker {
    txs: HashMap<B256, TxProvisioningState>,
}

impl Default for ProvisioningTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvisioningTracker {
    pub fn new() -> Self {
        Self {
            txs: HashMap::new(),
        }
    }

    /// Register a new transaction for provisioning.
    pub fn register(
        &mut self,
        tx_hash: B256,
        required_chunks: HashSet<ChunkKey>,
    ) -> &mut TxProvisioningState {
        self.txs
            .entry(tx_hash)
            .or_insert_with(|| TxProvisioningState {
                tx_hash,
                required_chunks,
                missing_chunks: HashSet::new(),
                state: ProvisioningState::Provisioning,
                started_at: Instant::now(),
            })
    }

    /// Get a transaction's provisioning state.
    pub fn get(&self, tx_hash: &B256) -> Option<&TxProvisioningState> {
        self.txs.get(tx_hash)
    }

    /// Get a mutable reference to a transaction's provisioning state.
    pub fn get_mut(&mut self, tx_hash: &B256) -> Option<&mut TxProvisioningState> {
        self.txs.get_mut(tx_hash)
    }

    /// Remove a transaction's provisioning state. Returns the removed state.
    pub fn remove(&mut self, tx_hash: &B256) -> Option<TxProvisioningState> {
        self.txs.remove(tx_hash)
    }

    /// Number of tracked transactions.
    pub fn len(&self) -> usize {
        self.txs.len()
    }

    /// Returns `true` if no transactions are being tracked.
    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(offset: u64) -> ChunkKey {
        ChunkKey { ledger: 0, offset }
    }

    #[test]
    fn test_register_and_query() {
        let mut tracker = ProvisioningTracker::new();
        let tx = B256::ZERO;
        let chunks = HashSet::from([make_key(1), make_key(2)]);

        tracker.register(tx, chunks.clone());

        let state = tracker.get(&tx).unwrap();
        assert_eq!(state.state, ProvisioningState::Provisioning);
        assert_eq!(state.required_chunks, chunks);
    }

    #[test]
    fn test_remove() {
        let mut tracker = ProvisioningTracker::new();
        let tx = B256::ZERO;
        tracker.register(tx, HashSet::from([make_key(1)]));

        let removed = tracker.remove(&tx);
        assert!(removed.is_some());
        assert!(tracker.get(&tx).is_none());
    }
}
