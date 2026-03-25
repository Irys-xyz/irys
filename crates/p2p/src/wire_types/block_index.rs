use std::fmt;

use irys_types::{block::DataLedger, H256};
use serde::{Deserialize, Serialize};

/// V1 wire type for [`irys_types::block::BlockIndexItem`].
///
/// Includes the redundant `num_ledgers` field for backwards compatibility
/// with V1 gossip peers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockIndexItemV1 {
    pub block_hash: H256,
    pub num_ledgers: u8,
    #[serde(
        serialize_with = "serialize_ledgers_positional",
        deserialize_with = "deserialize_ledgers_compat"
    )]
    pub ledgers: Vec<LedgerIndexItem>,
}

/// V2 wire type for [`irys_types::block::BlockIndexItem`].
///
/// `num_ledgers` is omitted — it is redundant with `ledgers.len()` and only
/// exists in the canonical type for compact binary encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockIndexItemV2 {
    pub block_hash: H256,
    #[serde(
        serialize_with = "serialize_ledgers_positional",
        deserialize_with = "deserialize_ledgers_compat"
    )]
    pub ledgers: Vec<LedgerIndexItem>,
}

/// Wire type for [`irys_types::block::LedgerIndexItem`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerIndexItem {
    #[serde(with = "irys_types::string_u64")]
    pub total_chunks: u64,
    pub tx_root: H256,
    pub ledger: DataLedger,
}

super::impl_mirror_from!(irys_types::block::LedgerIndexItem => LedgerIndexItem {
    total_chunks, tx_root, ledger,
});

// --- Backward-compatible serialization / deserialization ------------------------
//
// TEMPORARY: remove once all nodes have been updated past the commit that added
// the `ledger` field to `LedgerIndexItem`.
//
// Old nodes use array position to determine the ledger type (`DataLedger::ALL[i]`).
// New nodes include an explicit `ledger` field but must still serialize entries in
// positional order so old nodes interpret them correctly.

/// Serialize `ledgers` sorted by `DataLedger` position so old nodes — which
/// derive the ledger type from array index — get the correct mapping.
fn serialize_ledgers_positional<S>(
    ledgers: &[LedgerIndexItem],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq as _;

    let mut sorted: Vec<&LedgerIndexItem> = ledgers.iter().collect();
    sorted.sort_by_key(|l| l.ledger as u32);

    let mut seq = serializer.serialize_seq(Some(sorted.len()))?;
    for item in sorted {
        seq.serialize_element(item)?;
    }
    seq.end()
}

// --- Backward-compatible deserialization ----------------------------------------
//
// Old nodes serialize `LedgerIndexItem` without a `ledger` field — the ledger
// type is implied by position in the parent array (`DataLedger::ALL[i]`).
// New nodes include the explicit `ledger` field.
//
// The helper below deserializes each element as a `LedgerIndexItemCompat` (where
// `ledger` is optional), then fills in the missing values from position.

/// Internal helper: same shape as [`LedgerIndexItem`] but with an optional
/// `ledger` field for backward compatibility with legacy payloads.
#[derive(Deserialize)]
struct LedgerIndexItemCompat {
    #[serde(with = "irys_types::string_u64")]
    total_chunks: u64,
    tx_root: H256,
    /// Absent in payloads from old nodes.
    ledger: Option<DataLedger>,
}

/// Deserialize a `Vec<LedgerIndexItem>`, inferring the `ledger` field from
/// array position when the sender omits it (pre-`ledger`-field nodes).
fn deserialize_ledgers_compat<'de, D>(deserializer: D) -> Result<Vec<LedgerIndexItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<LedgerIndexItemCompat> = Vec::deserialize(deserializer)?;
    raw.into_iter()
        .enumerate()
        .map(|(idx, item)| {
            let ledger = match item.ledger {
                Some(l) => l,
                None => *DataLedger::ALL.get(idx).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "legacy payload has {count} ledgers but DataLedger::ALL \
                         only has {max} entries (index {idx} out of range)",
                        count = idx + 1,
                        max = DataLedger::ALL.len(),
                    ))
                })?,
            };
            Ok(LedgerIndexItem {
                total_chunks: item.total_chunks,
                tx_root: item.tx_root,
                ledger,
            })
        })
        .collect()
}

// --- BlockIndexItemV1 conversions (preserves num_ledgers) ---

super::impl_mirror_from!(irys_types::block::BlockIndexItem => BlockIndexItemV1 {
    block_hash, num_ledgers,
} convert_iter { ledgers });

// --- BlockIndexItemV2 conversions (derives num_ledgers from ledgers.len()) ---

impl From<irys_types::block::BlockIndexItem> for BlockIndexItemV2 {
    fn from(src: irys_types::block::BlockIndexItem) -> Self {
        Self {
            block_hash: src.block_hash,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}

/// Error returned when a [`BlockIndexItemV2`] cannot be converted to
/// the canonical [`irys_types::block::BlockIndexItem`].
#[derive(Debug, Clone)]
pub struct BlockIndexItemV2ConversionError {
    pub ledger_count: usize,
}

impl fmt::Display for BlockIndexItemV2ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ledger count {} exceeds u8::MAX; protocol supports at most 255 ledgers",
            self.ledger_count
        )
    }
}

impl std::error::Error for BlockIndexItemV2ConversionError {}

impl TryFrom<BlockIndexItemV2> for irys_types::block::BlockIndexItem {
    type Error = BlockIndexItemV2ConversionError;

    fn try_from(src: BlockIndexItemV2) -> Result<Self, Self::Error> {
        let num_ledgers =
            u8::try_from(src.ledgers.len()).map_err(|_| BlockIndexItemV2ConversionError {
                ledger_count: src.ledgers.len(),
            })?;
        Ok(Self {
            block_hash: src.block_hash,
            num_ledgers,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old nodes serialize LedgerIndexItem without the `ledger` field.
    /// Position in the array determines the ledger type.
    #[test]
    fn deserialize_legacy_block_index_item_v1() {
        // H256 is base58-encoded in JSON; these are the base58 representations of
        // deterministic test hashes (the exact values don't matter for this test).
        let json = r#"{
            "block_hash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
            "num_ledgers": 2,
            "ledgers": [
                { "total_chunks": "100", "tx_root": "HHTEVaETWsabcpHK7zcS7xzqqGhWKKTcfLd93boP4HjN" },
                { "total_chunks": "200", "tx_root": "HMNXdshU7AspkuXpZHwMQqmc5RuhzP9SDkHo6yqyod45" }
            ]
        }"#;

        let item: BlockIndexItemV1 = serde_json::from_str(json).unwrap();
        assert_eq!(item.ledgers.len(), 2);
        assert_eq!(item.ledgers[0].ledger, DataLedger::Publish);
        assert_eq!(item.ledgers[0].total_chunks, 100);
        assert_eq!(item.ledgers[1].ledger, DataLedger::Submit);
        assert_eq!(item.ledgers[1].total_chunks, 200);
    }

    /// New nodes include the explicit `ledger` field — it must be respected.
    /// Ledger order in the JSON is intentionally swapped relative to
    /// `DataLedger::ALL` so that the test fails if the code infers ledger
    /// from array position instead of honouring the explicit field.
    #[test]
    fn deserialize_new_block_index_item_v1() {
        let json = r#"{
            "block_hash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
            "num_ledgers": 2,
            "ledgers": [
                { "total_chunks": "100", "tx_root": "HHTEVaETWsabcpHK7zcS7xzqqGhWKKTcfLd93boP4HjN", "ledger": "Submit" },
                { "total_chunks": "200", "tx_root": "HMNXdshU7AspkuXpZHwMQqmc5RuhzP9SDkHo6yqyod45", "ledger": "Publish" }
            ]
        }"#;

        let item: BlockIndexItemV1 = serde_json::from_str(json).unwrap();
        assert_eq!(item.ledgers[0].ledger, DataLedger::Submit);
        assert_eq!(item.ledgers[1].ledger, DataLedger::Publish);
    }

    /// Serialization always includes the `ledger` field (new format).
    #[test]
    fn serialize_includes_ledger_field() {
        let item = BlockIndexItemV1 {
            block_hash: H256::zero(),
            num_ledgers: 1,
            ledgers: vec![LedgerIndexItem {
                total_chunks: 42,
                tx_root: H256::zero(),
                ledger: DataLedger::Submit,
            }],
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"ledger\":\"Submit\""));
    }

    /// Ledgers are serialized in `DataLedger` positional order regardless of
    /// the order they appear in the vec, so old nodes (which use array index
    /// to determine ledger type) get the correct mapping.
    #[test]
    fn serialize_sorts_ledgers_by_position() {
        let item = BlockIndexItemV1 {
            block_hash: H256::zero(),
            num_ledgers: 2,
            ledgers: vec![
                // Intentionally reversed: Submit first, Publish second.
                LedgerIndexItem {
                    total_chunks: 200,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
                LedgerIndexItem {
                    total_chunks: 100,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
            ],
        };

        let json = serde_json::to_string(&item).unwrap();
        // After serialization, Publish (position 0) must come before Submit (position 1).
        let publish_pos = json.find("\"total_chunks\":\"100\"").unwrap();
        let submit_pos = json.find("\"total_chunks\":\"200\"").unwrap();
        assert!(
            publish_pos < submit_pos,
            "Publish entry must be serialized before Submit entry"
        );
    }

    /// V2 round-trip: serialize then deserialize preserves all fields.
    /// Exercises the `serialize_ledgers_positional` / `deserialize_ledgers_compat`
    /// hooks on BlockIndexItemV2.
    #[test]
    fn roundtrip_block_index_item_v2() {
        let original = BlockIndexItemV2 {
            block_hash: H256::zero(),
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 100,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 200,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
            ],
        };

        let json = serde_json::to_string(&original).unwrap();
        let deserialized: BlockIndexItemV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    /// A legacy payload (no `ledger` field) with more entries than
    /// `DataLedger::ALL.len()` must produce a custom deserialization error.
    #[test]
    fn deserialize_oversized_legacy_payload_errors() {
        // DataLedger::ALL has 4 entries (Publish, Submit, OneYear, ThirtyDay).
        // Build a legacy payload with 5 ledgers (no `ledger` field), so the
        // fifth entry triggers the out-of-range error.
        let count = DataLedger::ALL.len() + 1;
        let ledger_entries: Vec<String> = (0..count)
            .map(|i| {
                format!(
                    r#"{{ "total_chunks": "{}", "tx_root": "HHTEVaETWsabcpHK7zcS7xzqqGhWKKTcfLd93boP4HjN" }}"#,
                    (i + 1) * 100
                )
            })
            .collect();
        let json = format!(
            r#"{{
            "block_hash": "DdqGmK5uamYN5vmuZrzpQhKeehLdwtPLVJdhu5P2iJKC",
            "num_ledgers": {count},
            "ledgers": [{}]
        }}"#,
            ledger_entries.join(", ")
        );

        let err = serde_json::from_str::<BlockIndexItemV1>(&json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("legacy payload has {count} ledgers"))
                && msg.contains(&format!("only has {} entries", DataLedger::ALL.len())),
            "expected malformed-legacy error, got: {msg}"
        );
    }
}
