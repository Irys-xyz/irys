use super::cache::ChunkKey;
use reth::revm::primitives::B256;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Number of blocks before a provisioning request expires.
const TTL_BLOCKS: u64 = 20;

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
    /// Block height at which this entry expires (unless locked).
    pub expire_height: Option<u64>,
}

/// Manages per-transaction provisioning state with TTL-based expiration.
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
        current_height: Option<u64>,
    ) -> &mut TxProvisioningState {
        let expire_height = current_height.map(|h| h.saturating_add(TTL_BLOCKS));

        self.txs
            .entry(tx_hash)
            .or_insert_with(|| TxProvisioningState {
                tx_hash,
                required_chunks,
                missing_chunks: HashSet::new(),
                state: ProvisioningState::Provisioning,
                started_at: Instant::now(),
                expire_height,
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

    /// Check if a transaction is ready for execution.
    pub fn is_ready(&self, tx_hash: &B256) -> bool {
        match self.txs.get(tx_hash) {
            Some(state) => matches!(state.state, ProvisioningState::Ready),
            // Unknown tx — return true to not block non-PD transactions
            None => true,
        }
    }

    /// Remove expired entries based on block height.
    /// Returns the tx hashes and required chunk keys of expired entries.
    pub fn expire_at_height(&mut self, height: u64) -> Vec<(B256, HashSet<ChunkKey>)> {
        let expired: Vec<B256> = self
            .txs
            .iter()
            .filter(|(_, state)| state.expire_height.is_some_and(|expire| expire <= height))
            .map(|(hash, _)| *hash)
            .collect();

        let mut result = Vec::with_capacity(expired.len());
        for tx_hash in expired {
            if let Some(state) = self.txs.remove(&tx_hash) {
                result.push((tx_hash, state.required_chunks));
            }
        }
        result
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

        tracker.register(tx, chunks.clone(), Some(100));

        let state = tracker.get(&tx).unwrap();
        assert_eq!(state.state, ProvisioningState::Provisioning);
        assert_eq!(state.required_chunks, chunks);
        assert_eq!(state.expire_height, Some(120));
    }

    #[test]
    fn test_is_ready_unknown_tx() {
        let tracker = ProvisioningTracker::new();
        // Unknown transactions should be considered ready (non-PD pass-through)
        assert!(tracker.is_ready(&B256::ZERO));
    }

    #[test]
    fn test_ready_state_flow() {
        let mut tracker = ProvisioningTracker::new();
        let tx = B256::ZERO;
        tracker.register(tx, HashSet::new(), Some(100));

        // Not ready while Provisioning
        assert!(!tracker.is_ready(&tx));

        // Transition to Ready
        tracker.get_mut(&tx).unwrap().state = ProvisioningState::Ready;
        assert!(tracker.is_ready(&tx));
    }

    #[test]
    fn test_expire_at_height() {
        let mut tracker = ProvisioningTracker::new();
        let tx1 = B256::ZERO;
        let tx2 = B256::with_last_byte(1);

        tracker.register(tx1, HashSet::from([make_key(1)]), Some(100));
        tracker.register(tx2, HashSet::from([make_key(2)]), Some(100));

        // Make tx1 Ready
        tracker.get_mut(&tx1).unwrap().state = ProvisioningState::Ready;

        // Expire at height 120 — both should expire
        let expired = tracker.expire_at_height(120);
        assert_eq!(expired.len(), 2);

        // Neither still tracked
        assert!(tracker.get(&tx1).is_none());
        assert!(tracker.get(&tx2).is_none());
    }

    #[test]
    fn test_remove() {
        let mut tracker = ProvisioningTracker::new();
        let tx = B256::ZERO;
        tracker.register(tx, HashSet::from([make_key(1)]), Some(100));

        let removed = tracker.remove(&tx);
        assert!(removed.is_some());
        assert!(tracker.get(&tx).is_none());
    }
}
