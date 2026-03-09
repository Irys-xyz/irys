use irys_types::{block::DataLedger, H256};
use serde::{Deserialize, Serialize};

/// Wire type for [`irys_types::block::BlockIndexItem`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockIndexItem {
    pub block_hash: H256,
    pub num_ledgers: u8,
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

impl From<&irys_types::block::BlockIndexItem> for BlockIndexItem {
    fn from(src: &irys_types::block::BlockIndexItem) -> Self {
        Self {
            block_hash: src.block_hash,
            num_ledgers: src.num_ledgers,
            ledgers: src.ledgers.iter().map(Into::into).collect(),
        }
    }
}

impl From<BlockIndexItem> for irys_types::block::BlockIndexItem {
    fn from(src: BlockIndexItem) -> Self {
        Self {
            block_hash: src.block_hash,
            num_ledgers: src.num_ledgers,
            ledgers: src.ledgers.into_iter().map(Into::into).collect(),
        }
    }
}
