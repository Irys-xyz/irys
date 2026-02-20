use irys_types::MempoolConsensusConfig;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct MempoolStatus {
    /// Total number of data transactions
    pub data_tx_count: usize,
    /// Total number of commitment transactions
    pub commitment_tx_count: usize,
    /// Total number of pending data root entries in the chunk ingress service.
    /// Populated by `AtomicMempoolState::get_status` via the provided `ChunkIngressState`.
    pub pending_chunks_count: usize,
    /// Total number of pending pledges
    pub pending_pledges_count: usize,
    /// Number of recently validated transactions
    pub recent_valid_tx_count: usize,
    /// Number of recently invalidated transactions
    pub recent_invalid_tx_count: usize,
    /// Total size of data transactions in bytes
    pub data_tx_total_size: u64,
    /// Memory pool configuration
    pub config: MempoolConsensusConfig,
    /// Capacity utilization percentage for data transactions (0-100)
    pub data_tx_capacity_pct: f64,
    /// Capacity utilization percentage for commitment addresses (0-100)
    pub commitment_address_capacity_pct: f64,
}
