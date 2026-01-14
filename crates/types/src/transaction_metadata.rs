use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

/// Metadata tracked for all transaction types
/// Stored separately from transaction headers to enable easier future migrations
/// This is NEVER serialized with the transaction - it's internal state only
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Compact,
    Serialize,
    Deserialize,
    arbitrary::Arbitrary,
)]
pub struct TransactionMetadata {
    /// Height of the block where this transaction was first included
    /// - For data txs: when included in Submit ledger
    /// - For commitment txs: when included in commitment ledger
    pub included_height: Option<u64>,
}

impl TransactionMetadata {
    pub fn new() -> Self {
        Self {
            included_height: None,
        }
    }

    pub fn with_included_height(height: u64) -> Self {
        Self {
            included_height: Some(height),
        }
    }

    pub fn is_included(&self) -> bool {
        self.included_height.is_some()
    }
}

/// Transaction status as exposed via API
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionStatus {
    /// Transaction is in mempool but not yet in any block
    Pending,
    /// Transaction is included in a block but the block hasn't been migrated to index yet
    Included,
    /// Transaction's block has been migrated to the block index (finalized)
    Confirmed,
}

/// API response for transaction status queries
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionStatusResponse {
    pub status: TransactionStatus,
    /// Only present if status is INCLUDED or CONFIRMED
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serialization::optional_string_u64"
    )]
    pub block_height: Option<u64>,
    /// Only present if status is INCLUDED or CONFIRMED
    /// Calculated as: current_head_height - block_height
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serialization::optional_string_u64"
    )]
    pub confirmations: Option<u64>,
}

impl TransactionStatusResponse {
    pub fn pending() -> Self {
        Self {
            status: TransactionStatus::Pending,
            block_height: None,
            confirmations: None,
        }
    }

    pub fn included(block_height: u64, current_head_height: u64) -> Self {
        Self {
            status: TransactionStatus::Included,
            block_height: Some(block_height),
            confirmations: Some(current_head_height.saturating_sub(block_height)),
        }
    }

    pub fn confirmed(block_height: u64, current_head_height: u64) -> Self {
        Self {
            status: TransactionStatus::Confirmed,
            block_height: Some(block_height),
            confirmations: Some(current_head_height.saturating_sub(block_height)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_creation() {
        let metadata = TransactionMetadata::new();
        assert!(!metadata.is_included());

        let metadata = TransactionMetadata::with_included_height(100);
        assert!(metadata.is_included());
        assert_eq!(metadata.included_height, Some(100));
    }

    #[test]
    fn test_status_response_creation() {
        let pending = TransactionStatusResponse::pending();
        assert_eq!(pending.status, TransactionStatus::Pending);
        assert!(pending.block_height.is_none());
        assert!(pending.confirmations.is_none());

        let included = TransactionStatusResponse::included(100, 110);
        assert_eq!(included.status, TransactionStatus::Included);
        assert_eq!(included.block_height, Some(100));
        assert_eq!(included.confirmations, Some(10));

        let confirmed = TransactionStatusResponse::confirmed(100, 110);
        assert_eq!(confirmed.status, TransactionStatus::Confirmed);
        assert_eq!(confirmed.block_height, Some(100));
        assert_eq!(confirmed.confirmations, Some(10));
    }
}
