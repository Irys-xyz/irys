use irys_types::{H256, block::DataLedger};
use serde::{Deserialize, Serialize};

/// V1 wire type for [`irys_types::block::BlockIndexItem`].
///
/// Includes `num_ledgers` for backward compatibility with V1 gossip peers.
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

// --- BlockIndexItemV1 conversions (derives num_ledgers from ledgers.len()) ---

impl From<irys_types::block::BlockIndexItem> for BlockIndexItemV1 {
    fn from(src: irys_types::block::BlockIndexItem) -> Self {
        Self {
            block_hash: src.block_hash,
            num_ledgers: u8::try_from(src.ledgers.len()).unwrap_or(u8::MAX),
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<BlockIndexItemV1> for irys_types::block::BlockIndexItem {
    fn from(src: BlockIndexItemV1) -> Self {
        Self {
            block_hash: src.block_hash,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}

/// V2 wire type for [`irys_types::block::BlockIndexItem`].
///
/// `num_ledgers` is omitted — the count is derived from `ledgers.len()`.
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
    if raw.len() > DataLedger::ALL.len() {
        return Err(serde::de::Error::custom(format!(
            "ledgers array too large: {} > {}",
            raw.len(),
            DataLedger::ALL.len(),
        )));
    }
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

// --- BlockIndexItemV2 conversions ---

super::impl_mirror_from!(irys_types::block::BlockIndexItem => BlockIndexItemV2 {
    block_hash,
} convert_iter { ledgers });

#[cfg(test)]
mod tests {
    use super::*;

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
}
