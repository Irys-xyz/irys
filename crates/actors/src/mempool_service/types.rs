use irys_types::{H256, IrysAddress, MempoolConsensusConfig};
use serde::{Deserialize, Serialize};

/// Default max entries returned per tx list when the client omits `?limit=`.
pub const MEMPOOL_TXS_DEFAULT_LIMIT: usize = 100;
/// Hard cap on `?limit=` for `GET /v1/mempool/txs` (prevents unbounded responses).
pub const MEMPOOL_TXS_MAX_LIMIT: usize = 500;

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

/// Pending data-tx entry for `GET /v1/mempool/txs` (id matches ledger / `/v1/tx/{id}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolPendingDataTx {
    pub id: H256,
    pub byte_size: u64,
    /// `byte_size.div_ceil(chunk_size)`.
    pub chunks: u64,
    pub data_root: H256,
    pub ledger_id: u32,
}

/// Pending commitment-tx entry for `GET /v1/mempool/txs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolPendingCommitmentTx {
    pub id: H256,
    pub address: IrysAddress,
}

/// `GET /v1/mempool/txs` response: unconfirmed txs only.
///
/// When `truncated`, arrays are capped and `total_*_tx_count` holds full totals.
/// Otherwise counts match array lengths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MempoolPendingTxs {
    pub data_txs: Vec<MempoolPendingDataTx>,
    pub commitment_txs: Vec<MempoolPendingCommitmentTx>,
    /// Always `data_txs.len()`.
    pub data_tx_count: usize,
    /// Always `commitment_txs.len()`.
    pub commitment_tx_count: usize,
    /// Same metric as `/v1/mempool/status`.
    pub pending_chunks_count: usize,
    /// True when either list has more after the current page.
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_data_tx_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_commitment_tx_count: Option<usize>,
}

impl MempoolPendingTxs {
    /// Empty pool (HTTP 200, not 404).
    #[must_use]
    pub fn empty(pending_chunks_count: usize) -> Self {
        Self {
            data_txs: Vec::new(),
            commitment_txs: Vec::new(),
            data_tx_count: 0,
            commitment_tx_count: 0,
            pending_chunks_count,
            truncated: false,
            total_data_tx_count: None,
            total_commitment_tx_count: None,
        }
    }
}
