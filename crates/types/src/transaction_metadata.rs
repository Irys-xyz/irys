use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

/// Metadata tracked for commitment transactions
/// Stored separately from transaction headers to enable easier future migrations
/// This is NEVER serialized with the transaction - it's internal state only
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    arbitrary::Arbitrary,
    Compact,
)]
pub struct CommitmentTransactionMetadata {
    /// Height of the block where this transaction was first included in the commitment ledger
    pub included_height: Option<u64>,
}

impl CommitmentTransactionMetadata {
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

    /// Merge metadata, preferring incoming fields when set, preserving existing otherwise
    pub fn merge(&self, incoming: &Self) -> Self {
        Self {
            included_height: incoming.included_height.or(self.included_height),
        }
    }
}

/// Metadata tracked for data transactions
/// Stored separately from transaction headers to enable easier future migrations
/// This is NEVER externally serialized with the transaction - it's internal state only
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    arbitrary::Arbitrary,
    Compact,
)]
pub struct DataTransactionMetadata {
    /// Height of the block where this transaction was first included in the Submit ledger
    pub included_height: Option<u64>,

    /// Height of the block where this transaction was promoted (submit -> publish)
    pub promoted_height: Option<u64>,
}

impl DataTransactionMetadata {
    pub fn new() -> Self {
        Self {
            included_height: None,
            promoted_height: None,
        }
    }

    pub fn with_included_height(height: u64) -> Self {
        Self {
            included_height: Some(height),
            promoted_height: None,
        }
    }

    /// Creates metadata with only a promoted height.
    /// This is used for testing or when you need to track promotion independently from inclusion.
    pub fn with_promoted_height(height: u64) -> Self {
        Self {
            included_height: None,
            promoted_height: Some(height),
        }
    }

    pub fn is_included(&self) -> bool {
        self.included_height.is_some()
    }

    pub fn is_promoted(&self) -> bool {
        self.promoted_height.is_some()
    }

    /// Merge metadata, preferring incoming fields when set, preserving existing otherwise
    pub fn merge(&self, incoming: &Self) -> Self {
        Self {
            included_height: incoming.included_height.or(self.included_height),
            promoted_height: incoming.promoted_height.or(self.promoted_height),
        }
    }
}

/// Transaction status as exposed via API
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransactionStatus {
    /// Transaction is in the mempool but not yet in any block
    Pending,
    /// Transaction is included in a block, but the block hasn't been migrated to index yet
    Confirmed,
    /// Transaction's block has been migrated to the block index (finalized)
    Finalized,
}

/// API response for transaction status queries
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionStatusResponse {
    pub status: TransactionStatus,
    /// Only present if status is MINED or FINALIZED
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serialization::optional_string_u64"
    )]
    pub block_height: Option<u64>,
    /// Only present if status is MINED or FINALIZED
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

    pub fn confirmed(block_height: u64, current_head_height: u64) -> Self {
        Self {
            status: TransactionStatus::Confirmed,
            block_height: Some(block_height),
            confirmations: Some(current_head_height.saturating_sub(block_height)),
        }
    }

    pub fn finalized(block_height: u64, current_head_height: u64) -> Self {
        Self {
            status: TransactionStatus::Finalized,
            block_height: Some(block_height),
            confirmations: Some(current_head_height.saturating_sub(block_height)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commitment_tx_metadata_creation() {
        let metadata = CommitmentTransactionMetadata::new();
        assert!(!metadata.is_included());

        let metadata = CommitmentTransactionMetadata::with_included_height(100);
        assert!(metadata.is_included());
        assert_eq!(metadata.included_height, Some(100));
    }

    #[test]
    fn test_data_tx_metadata_creation() {
        let metadata = DataTransactionMetadata::new();
        assert!(!metadata.is_included());
        assert!(!metadata.is_promoted());

        let metadata = DataTransactionMetadata::with_included_height(100);
        assert!(metadata.is_included());
        assert_eq!(metadata.included_height, Some(100));
        assert!(!metadata.is_promoted());

        let metadata = DataTransactionMetadata::with_promoted_height(42);
        assert!(!metadata.is_included());
        assert!(metadata.is_promoted());
        assert_eq!(metadata.promoted_height, Some(42));
    }

    #[test]
    fn test_status_response_creation() {
        let pending = TransactionStatusResponse::pending();
        assert_eq!(pending.status, TransactionStatus::Pending);
        assert!(pending.block_height.is_none());
        assert!(pending.confirmations.is_none());

        let included = TransactionStatusResponse::confirmed(100, 110);
        assert_eq!(included.status, TransactionStatus::Confirmed);
        assert_eq!(included.block_height, Some(100));
        assert_eq!(included.confirmations, Some(10));

        let confirmed = TransactionStatusResponse::finalized(100, 110);
        assert_eq!(confirmed.status, TransactionStatus::Finalized);
        assert_eq!(confirmed.block_height, Some(100));
        assert_eq!(confirmed.confirmations, Some(10));
    }

    #[test]
    fn test_commitment_metadata_compact_encoding_roundtrip() {
        // Test 1: included_height set
        let original = CommitmentTransactionMetadata {
            included_height: Some(100),
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = CommitmentTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded, original);
        assert!(rest.is_empty());

        // Test 2: No height set (default)
        let original = CommitmentTransactionMetadata {
            included_height: None,
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = CommitmentTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded, original);
        assert!(rest.is_empty());
    }

    #[test]
    fn test_data_metadata_compact_encoding_roundtrip() {
        // Test 1: Both heights set
        let original = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: Some(200),
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = DataTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded, original);
        assert!(rest.is_empty());

        // Test 2: Only included_height set
        let original = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: None,
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = DataTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded.included_height, Some(100));
        assert_eq!(decoded.promoted_height, None);
        assert_eq!(decoded, original);
        assert!(rest.is_empty());

        // Test 3: Only promoted_height set
        let original = DataTransactionMetadata {
            included_height: None,
            promoted_height: Some(200),
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = DataTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded.included_height, None);
        assert_eq!(decoded.promoted_height, Some(200));
        assert_eq!(decoded, original);
        assert!(rest.is_empty());

        // Test 4: Neither height set (default)
        let original = DataTransactionMetadata {
            included_height: None,
            promoted_height: None,
        };
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);
        let (decoded, rest) = DataTransactionMetadata::from_compact(&buf, buf.len());
        assert_eq!(decoded.included_height, None);
        assert_eq!(decoded.promoted_height, None);
        assert_eq!(decoded, original);
        assert!(rest.is_empty());
    }

    #[test]
    fn test_confirmations_saturating() {
        // Test edge case where block_height > current_head_height
        let response = TransactionStatusResponse::confirmed(110, 100);
        assert_eq!(response.confirmations, Some(0));

        // Normal case
        let response = TransactionStatusResponse::confirmed(100, 110);
        assert_eq!(response.confirmations, Some(10));

        // Same height
        let response = TransactionStatusResponse::confirmed(100, 100);
        assert_eq!(response.confirmations, Some(0));
    }

    #[test]
    fn test_commitment_metadata_merge() {
        // Test 1: Merge with incoming having data
        let existing = CommitmentTransactionMetadata {
            included_height: None,
        };
        let incoming = CommitmentTransactionMetadata {
            included_height: Some(100),
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(100));

        // Test 2: Merge preserves existing when incoming is None
        let existing = CommitmentTransactionMetadata {
            included_height: Some(100),
        };
        let incoming = CommitmentTransactionMetadata {
            included_height: None,
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(100));

        // Test 3: Incoming overrides existing when both have values
        let existing = CommitmentTransactionMetadata {
            included_height: Some(100),
        };
        let incoming = CommitmentTransactionMetadata {
            included_height: Some(200),
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(200));

        // Test 4: Both None stays None
        let existing = CommitmentTransactionMetadata {
            included_height: None,
        };
        let incoming = CommitmentTransactionMetadata {
            included_height: None,
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, None);
    }

    #[test]
    fn test_data_metadata_merge() {
        // Test 1: Merge with incoming having both fields
        let existing = DataTransactionMetadata {
            included_height: None,
            promoted_height: None,
        };
        let incoming = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: Some(200),
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(100));
        assert_eq!(merged.promoted_height, Some(200));

        // Test 2: Merge preserves existing when incoming has None
        let existing = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: Some(200),
        };
        let incoming = DataTransactionMetadata {
            included_height: None,
            promoted_height: None,
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(100));
        assert_eq!(merged.promoted_height, Some(200));

        // Test 3: Mixed scenario - incoming has included, existing has promoted
        let existing = DataTransactionMetadata {
            included_height: None,
            promoted_height: Some(200),
        };
        let incoming = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: None,
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(100));
        assert_eq!(merged.promoted_height, Some(200));

        // Test 4: Incoming overrides existing when both have values
        let existing = DataTransactionMetadata {
            included_height: Some(100),
            promoted_height: Some(200),
        };
        let incoming = DataTransactionMetadata {
            included_height: Some(150),
            promoted_height: Some(250),
        };
        let merged = existing.merge(&incoming);
        assert_eq!(merged.included_height, Some(150));
        assert_eq!(merged.promoted_height, Some(250));
    }
}
