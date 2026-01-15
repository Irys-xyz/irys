use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

/// Metadata tracked for all transaction types
/// Stored separately from transaction headers to enable easier future migrations
/// This is NEVER serialized with the transaction - it's internal state only
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, arbitrary::Arbitrary,
)]
pub struct TransactionMetadata {
    /// Height of the block where this transaction was first included
    /// - For data txs: when included in Submit ledger
    /// - For commitment txs: when included in commitment ledger
    pub included_height: Option<u64>,

    /// Height of the block where this transaction was promoted (submit -> publish)
    /// - Only applicable to data txs
    pub promoted_height: Option<u64>,
}

// NOTE: manual `Compact` impl to keep backward-compatibility with older DB values that
// only encoded `included_height`.
impl Compact for TransactionMetadata {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let mut size = 0;
        size += Compact::to_compact(&self.included_height, buf);
        size += Compact::to_compact(&self.promoted_height, buf);
        size
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (included_height, rest) = Option::<u64>::from_compact(buf, buf.len());

        // Back-compat: older encodings may end after `included_height`.
        let (promoted_height, rest) = if rest.is_empty() {
            (None, rest)
        } else {
            Option::<u64>::from_compact(rest, rest.len())
        };

        (
            Self {
                included_height,
                promoted_height,
            },
            rest,
        )
    }
}

impl TransactionMetadata {
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

    /// Constructor for the common case where a transaction is both included and promoted
    /// at the same height (e.g., single-block promotion)
    pub fn with_promoted_height(height: u64) -> Self {
        Self {
            included_height: Some(height),
            promoted_height: Some(height),
        }
    }

    pub fn is_included(&self) -> bool {
        self.included_height.is_some()
    }

    pub fn is_promoted(&self) -> bool {
        self.promoted_height.is_some()
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
        assert!(!metadata.is_promoted());

        let metadata = TransactionMetadata::with_included_height(100);
        assert!(metadata.is_included());
        assert_eq!(metadata.included_height, Some(100));
        assert!(!metadata.is_promoted());

        let metadata = TransactionMetadata::with_promoted_height(42);
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

        let included = TransactionStatusResponse::included(100, 110);
        assert_eq!(included.status, TransactionStatus::Included);
        assert_eq!(included.block_height, Some(100));
        assert_eq!(included.confirmations, Some(10));

        let confirmed = TransactionStatusResponse::confirmed(100, 110);
        assert_eq!(confirmed.status, TransactionStatus::Confirmed);
        assert_eq!(confirmed.block_height, Some(100));
        assert_eq!(confirmed.confirmations, Some(10));
    }

    #[test]
    fn test_compact_encoding_roundtrip() {
        // Create metadata with both heights
        let original = TransactionMetadata {
            included_height: Some(100),
            promoted_height: Some(200),
        };

        // Encode
        let mut buf = Vec::new();
        let size = Compact::to_compact(&original, &mut buf);
        assert!(size > 0);

        // Decode
        let (decoded, rest) = TransactionMetadata::from_compact(&buf, buf.len());

        // Verify
        assert_eq!(decoded, original);
        assert!(rest.is_empty());
    }

    #[test]
    fn test_compact_backward_compatibility() {
        // Simulate older encoding that only has included_height
        let mut buf = Vec::new();
        Compact::to_compact(&Some(100u64), &mut buf);

        // Decode with new implementation
        let (decoded, rest) = TransactionMetadata::from_compact(&buf, buf.len());

        // Verify backward compatibility
        assert_eq!(decoded.included_height, Some(100));
        assert_eq!(decoded.promoted_height, None);
        assert!(rest.is_empty());
    }

    #[test]
    fn test_confirmations_saturating() {
        // Test edge case where block_height > current_head_height
        let response = TransactionStatusResponse::included(110, 100);
        assert_eq!(response.confirmations, Some(0));

        // Normal case
        let response = TransactionStatusResponse::included(100, 110);
        assert_eq!(response.confirmations, Some(10));

        // Same height
        let response = TransactionStatusResponse::included(100, 100);
        assert_eq!(response.confirmations, Some(0));
    }
}

/// Wrapper for data transactions with metadata
/// This ensures metadata travels with the transaction and gets cleaned up automatically
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataTxWithMetadata {
    pub tx: crate::DataTransactionHeader,
    pub metadata: TransactionMetadata,
}

impl DataTxWithMetadata {
    pub fn new(tx: crate::DataTransactionHeader) -> Self {
        Self {
            tx,
            metadata: TransactionMetadata::new(),
        }
    }

    pub fn with_metadata(tx: crate::DataTransactionHeader, metadata: TransactionMetadata) -> Self {
        Self { tx, metadata }
    }

    pub fn with_included_height(tx: crate::DataTransactionHeader, height: u64) -> Self {
        Self {
            tx,
            metadata: TransactionMetadata::with_included_height(height),
        }
    }

    /// Get the transaction ID
    pub fn id(&self) -> crate::H256 {
        self.tx.id
    }

    /// Set the included height
    pub fn set_included_height(&mut self, height: u64) {
        self.metadata.included_height = Some(height);
    }

    /// Set the promoted height
    pub fn set_promoted_height(&mut self, height: u64) {
        self.metadata.promoted_height = Some(height);
    }

    /// Clear the included height (used during re-orgs)
    pub fn clear_included_height(&mut self) {
        self.metadata.included_height = None;
    }

    /// Clear the promoted height (used during re-orgs)
    pub fn clear_promoted_height(&mut self) {
        self.metadata.promoted_height = None;
    }

    /// Split into transaction and metadata for database storage
    pub fn split(self) -> (crate::DataTransactionHeader, TransactionMetadata) {
        (self.tx, self.metadata)
    }

    /// Get references to both parts
    pub fn as_parts(&self) -> (&crate::DataTransactionHeader, &TransactionMetadata) {
        (&self.tx, &self.metadata)
    }
}

/// Implement Deref so we can access transaction fields directly
impl std::ops::Deref for DataTxWithMetadata {
    type Target = crate::DataTransactionHeader;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

/// Implement DerefMut so we can mutate transaction fields directly
impl std::ops::DerefMut for DataTxWithMetadata {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}
