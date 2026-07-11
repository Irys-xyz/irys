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

/// Light metadata for a pending data transaction in the mempool list API.
///
/// IDs use the same base58 encoding as block ledger `txIds` and `GET /v1/tx/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolPendingDataTx {
    /// Transaction id (base58 `H256`).
    pub id: H256,
    /// Declared payload size in bytes (`data_size`).
    pub byte_size: u64,
    /// Number of chunks implied by `byte_size` and consensus `chunk_size`
    /// (`byte_size.div_ceil(chunk_size)`).
    pub chunks: u64,
    /// Merkle root of the transaction data chunks.
    pub data_root: H256,
    /// Destination ledger id (e.g. Submit / term ledgers).
    pub ledger_id: u32,
}

/// Light metadata for a pending commitment transaction in the mempool list API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MempoolPendingCommitmentTx {
    /// Transaction id (base58 `H256`).
    pub id: H256,
    /// Signer address.
    pub address: IrysAddress,
}

/// Response body for `GET /v1/mempool/txs`.
///
/// Lists **unconfirmed** mempool transactions only (no `included_height` /
/// `promoted_height` yet). Confirmed txs may still sit in mempool state for
/// reorg handling; they are excluded so the list reflects txs still awaiting
/// inclusion.
///
/// When `truncated` is true, `data_txs` / `commitment_txs` are capped by the
/// request limit and `total_*_tx_count` carries the full unconfirmed totals.
/// When not truncated, `data_tx_count` / `commitment_tx_count` match the array
/// lengths (and equal the totals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MempoolPendingTxs {
    pub data_txs: Vec<MempoolPendingDataTx>,
    pub commitment_txs: Vec<MempoolPendingCommitmentTx>,
    /// Number of data txs in `data_txs` (always equals `data_txs.len()`).
    pub data_tx_count: usize,
    /// Number of commitment txs in `commitment_txs` (always equals `commitment_txs.len()`).
    pub commitment_tx_count: usize,
    /// Pending chunk-ingress entries (same metric as `/v1/mempool/status`).
    pub pending_chunks_count: usize,
    /// True when either list was cut short by the limit.
    pub truncated: bool,
    /// Full unconfirmed data-tx count when `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_data_tx_count: Option<usize>,
    /// Full unconfirmed commitment-tx count when `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_commitment_tx_count: Option<usize>,
}

impl MempoolPendingTxs {
    /// Empty mempool response (HTTP 200, not 404).
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
