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
    pub ledgers: Vec<LedgerIndexItem>,
}

/// V2 wire type for [`irys_types::block::BlockIndexItem`].
///
/// `num_ledgers` is omitted — it is redundant with `ledgers.len()` and only
/// exists in the canonical type for compact binary encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockIndexItemV2 {
    pub block_hash: H256,
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

// --- BlockIndexItemV1 conversions (preserves num_ledgers) ---

impl From<irys_types::block::BlockIndexItem> for BlockIndexItemV1 {
    fn from(src: irys_types::block::BlockIndexItem) -> Self {
        Self {
            block_hash: src.block_hash,
            num_ledgers: src.num_ledgers,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<BlockIndexItemV1> for irys_types::block::BlockIndexItem {
    fn from(src: BlockIndexItemV1) -> Self {
        Self {
            block_hash: src.block_hash,
            num_ledgers: src.num_ledgers,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}

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
        let num_ledgers = u8::try_from(src.ledgers.len()).map_err(|_| {
            BlockIndexItemV2ConversionError {
                ledger_count: src.ledgers.len(),
            }
        })?;
        Ok(Self {
            block_hash: src.block_hash,
            num_ledgers,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        })
    }
}
