use irys_types::{H256, IrysAddress, MempoolConsensusConfig};
use serde::{Deserialize, Serialize};

/// Default max entries returned per tx list when the client omits `?limit=`.
pub const MEMPOOL_TXS_DEFAULT_LIMIT: usize = 100;
/// Hard cap on `?limit=` for `GET /v1/mempool/txs` (prevents unbounded responses).
pub const MEMPOOL_TXS_MAX_LIMIT: usize = 500;

/// Independent forward positions for data and commitment lists.
///
/// Opaque wire form: `v1.<data_b58|_>.<commitment_b58|_>` (`_` = no bound).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MempoolTxsCursor {
    pub after_data_id: Option<H256>,
    pub after_commitment_id: Option<H256>,
}

impl MempoolTxsCursor {
    const PREFIX: &'static str = "v1";
    const NONE: &'static str = "_";

    /// Encode for `?cursor=` / `next_cursor`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}.{}.{}",
            Self::PREFIX,
            self.after_data_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| Self::NONE.to_owned()),
            self.after_commitment_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| Self::NONE.to_owned()),
        )
    }

    /// Parse `?cursor=`. Rejects unknown versions / malformed ids.
    pub fn decode(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 || parts[0] != Self::PREFIX {
            return Err("expected v1.<data|_>.<commitment|_>".into());
        }
        Ok(Self {
            after_data_id: Self::parse_part(parts[1], "data")?,
            after_commitment_id: Self::parse_part(parts[2], "commitment")?,
        })
    }

    fn parse_part(part: &str, label: &str) -> Result<Option<H256>, String> {
        if part == Self::NONE || part.is_empty() {
            return Ok(None);
        }
        H256::from_base58_result(part)
            .map(Some)
            .map_err(|e| format!("invalid {label} id in cursor: {e}"))
    }

    /// Advance each side to the last id returned on that page (keep prior if empty).
    #[must_use]
    pub fn advance(
        self,
        data_page: &[MempoolPendingDataTx],
        commitment_page: &[MempoolPendingCommitmentTx],
    ) -> Self {
        Self {
            after_data_id: data_page.last().map(|t| t.id).or(self.after_data_id),
            after_commitment_id: commitment_page
                .last()
                .map(|t| t.id)
                .or(self.after_commitment_id),
        }
    }
}

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
/// When `truncated`, arrays are capped, `total_*_tx_count` holds full totals, and
/// `next_cursor` pages each list independently (no shared single-id cursor).
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
    /// Opaque dual-list cursor for the next page (`?cursor=`). Present iff `truncated`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
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
            next_cursor: None,
            total_data_tx_count: None,
            total_commitment_tx_count: None,
        }
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn cursor_round_trips_both_bounds() {
        let c = MempoolTxsCursor {
            after_data_id: Some(H256([1; 32])),
            after_commitment_id: Some(H256([2; 32])),
        };
        assert_eq!(MempoolTxsCursor::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn cursor_round_trips_none_markers() {
        let c = MempoolTxsCursor::default();
        assert_eq!(c.encode(), "v1._._");
        assert_eq!(MempoolTxsCursor::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn cursor_rejects_bad_version() {
        assert!(MempoolTxsCursor::decode("v0._._").is_err());
        assert!(MempoolTxsCursor::decode("not-a-cursor").is_err());
    }

    #[test]
    fn cursor_advance_keeps_prior_when_page_empty() {
        let prev = MempoolTxsCursor {
            after_data_id: Some(H256([9; 32])),
            after_commitment_id: None,
        };
        let advanced = prev.advance(&[], &[]);
        assert_eq!(advanced.after_data_id, Some(H256([9; 32])));
        assert_eq!(advanced.after_commitment_id, None);
    }
}
