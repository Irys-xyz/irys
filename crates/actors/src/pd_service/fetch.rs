use irys_types::{ChunkFormat, IrysAddress};
use reth::revm::primitives::B256;
use std::collections::HashSet;
use tokio::task::AbortHandle;
use tokio_util::time::delay_queue;

use super::cache::ChunkKey;

/// Result returned by a chunk fetch task in the JoinSet.
pub(crate) struct PdChunkFetchResult {
    pub key: ChunkKey,
    pub result: Result<ChunkFormat, PdChunkFetchError>,
}

/// Errors from a PD chunk fetch attempt.
#[derive(Debug)]
pub(crate) enum PdChunkFetchError {
    /// All assigned peers tried and failed.
    AllPeersFailed {
        excluded_peers: HashSet<IrysAddress>,
    },
    /// Chunk verification failed (data_root mismatch, proof invalid, etc.).
    VerificationFailed,
}

/// Phase of a single chunk key's fetch lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchPhase {
    /// An HTTP fetch task is running in the JoinSet.
    Fetching,
    /// Waiting for the backoff timer to expire in the DelayQueue.
    Backoff,
}

/// Tracks an in-flight or retrying fetch for a single ChunkKey.
pub(crate) struct PdChunkFetchState {
    /// Mempool transactions waiting on this chunk.
    pub waiting_txs: HashSet<B256>,
    /// Block validations waiting on this chunk.
    pub waiting_blocks: HashSet<B256>,
    /// Number of fetch attempts so far.
    pub attempt: u32,
    /// Distinguishes provisioning lifecycles. Prevents stale retry timers.
    pub generation: u64,
    /// Peers that have been tried and failed.
    pub excluded_peers: HashSet<IrysAddress>,
    /// Current phase of the fetch lifecycle.
    pub status: FetchPhase,
    /// Handle to cancel the in-flight fetch task (when Fetching).
    pub abort_handle: Option<AbortHandle>,
    /// Handle to cancel the pending retry timer (when Backoff).
    pub retry_queue_key: Option<delay_queue::Key>,
}

/// Entry stored in the DelayQueue for scheduled retries.
pub(crate) struct RetryEntry {
    pub key: ChunkKey,
    pub attempt: u32,
    pub generation: u64,
    pub excluded_peers: HashSet<IrysAddress>,
}

/// Tracks a block validation waiting for chunks to be fetched.
pub(crate) struct PendingBlockProvision {
    /// Chunks still missing for this block.
    pub remaining_keys: HashSet<ChunkKey>,
    /// All chunk keys for this block (for reference cleanup).
    pub all_keys: Vec<ChunkKey>,
    /// The oneshot sender to respond on when all chunks arrive.
    pub response: tokio::sync::oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
}

/// Maximum number of retry attempts per chunk key before permanent failure.
pub(crate) const MAX_CHUNK_FETCH_RETRIES: u32 = 10;

/// Computes the exponential backoff duration for a given attempt.
/// Formula: min(1s * 2^attempt, 30s)
pub(crate) fn backoff_duration(attempt: u32) -> std::time::Duration {
    let secs = 1_u64.saturating_mul(1_u64.checked_shl(attempt).unwrap_or(u64::MAX));
    std::time::Duration::from_secs(secs.min(30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_duration_exponential_capped_at_30s() {
        assert_eq!(backoff_duration(0), std::time::Duration::from_secs(1));
        assert_eq!(backoff_duration(1), std::time::Duration::from_secs(2));
        assert_eq!(backoff_duration(2), std::time::Duration::from_secs(4));
        assert_eq!(backoff_duration(3), std::time::Duration::from_secs(8));
        assert_eq!(backoff_duration(4), std::time::Duration::from_secs(16));
        assert_eq!(backoff_duration(5), std::time::Duration::from_secs(30));
        assert_eq!(backoff_duration(6), std::time::Duration::from_secs(30));
        assert_eq!(backoff_duration(32), std::time::Duration::from_secs(30));
        assert_eq!(
            backoff_duration(u32::MAX),
            std::time::Duration::from_secs(30)
        );
    }
}
