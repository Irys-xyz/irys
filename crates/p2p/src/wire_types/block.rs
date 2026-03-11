use irys_types::{
    block::IrysTokenPrice,
    partition::PartitionHash,
    serialization::{Base64, H256List, IngressProofsList},
    BlockHash, IrysAddress, IrysSignature, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};

use super::{impl_json_version_tagged_serde, CommitmentTransaction, DataTransactionHeader};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PoaData {
    pub partition_chunk_offset: u32,
    pub partition_hash: PartitionHash,
    pub chunk: Option<Base64>,
    pub ledger_id: Option<u32>,
    pub tx_path: Option<Base64>,
    pub data_path: Option<Base64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VDFLimiterInfo {
    pub output: H256,
    #[serde(with = "irys_types::string_u64")]
    pub global_step_number: u64,
    pub seed: H256,
    pub next_seed: H256,
    pub prev_output: H256,
    pub last_step_checkpoints: H256List,
    pub steps: H256List,
    #[serde(default, with = "irys_types::option_u64_stringify")]
    pub vdf_difficulty: Option<u64>,
    #[serde(default, with = "irys_types::option_u64_stringify")]
    pub next_vdf_difficulty: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataTransactionLedger {
    pub ledger_id: u32,
    pub tx_root: H256,
    pub tx_ids: H256List,
    #[serde(default, with = "irys_types::u64_stringify")]
    pub total_chunks: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "irys_types::optional_string_u64"
    )]
    pub expires: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<IngressProofsList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_proof_count: Option<u8>,
}

/// Wire type for SystemTransactionLedger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemTransactionLedger {
    pub ledger_id: u32,
    pub tx_ids: H256List,
}

/// Inner fields for IrysBlockHeaderV1
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IrysBlockHeaderV1Inner {
    pub block_hash: BlockHash,
    pub signature: IrysSignature,
    #[serde(with = "irys_types::string_u64")]
    pub height: u64,
    pub diff: U256,
    pub cumulative_diff: U256,
    pub solution_hash: H256,
    pub last_diff_timestamp: UnixTimestampMs,
    pub previous_solution_hash: H256,
    pub last_epoch_hash: H256,
    pub chunk_hash: H256,
    pub previous_block_hash: H256,
    pub previous_cumulative_diff: U256,
    pub poa: PoaData,
    pub reward_address: IrysAddress,
    pub reward_amount: U256,
    pub miner_address: IrysAddress,
    pub timestamp: UnixTimestampMs,
    pub system_ledgers: Vec<SystemTransactionLedger>,
    pub data_ledgers: Vec<DataTransactionLedger>,
    pub evm_block_hash: B256,
    pub vdf_limiter_info: VDFLimiterInfo,
    pub oracle_irys_price: IrysTokenPrice,
    pub ema_irys_price: IrysTokenPrice,
    pub treasury: U256,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IrysBlockHeader {
    V1(IrysBlockHeaderV1Inner),
}

impl_json_version_tagged_serde!(IrysBlockHeader { 1 => V1(IrysBlockHeaderV1Inner) });

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockBody {
    pub block_hash: BlockHash,
    pub data_transactions: Vec<DataTransactionHeader>,
    pub commitment_transactions: Vec<CommitmentTransaction>,
}

// conversions (mirror structs)

super::impl_mirror_from!(irys_types::PoaData => PoaData {
    partition_chunk_offset, partition_hash, chunk, ledger_id, tx_path, data_path,
});

super::impl_mirror_from!(irys_types::VDFLimiterInfo => VDFLimiterInfo {
    output, global_step_number, seed, next_seed, prev_output,
    last_step_checkpoints, steps, vdf_difficulty, next_vdf_difficulty,
});

super::impl_mirror_from!(irys_types::DataTransactionLedger => DataTransactionLedger {
    ledger_id, tx_root, tx_ids, total_chunks, expires, proofs, required_proof_count,
});

super::impl_mirror_from!(irys_types::SystemTransactionLedger => SystemTransactionLedger {
    ledger_id, tx_ids,
});

super::impl_mirror_from!(irys_types::IrysBlockHeaderV1 => IrysBlockHeaderV1Inner {
    block_hash, signature, height, diff, cumulative_diff, solution_hash,
    last_diff_timestamp, previous_solution_hash, last_epoch_hash, chunk_hash,
    previous_block_hash, previous_cumulative_diff, reward_address, reward_amount,
    miner_address, timestamp, evm_block_hash, oracle_irys_price, ema_irys_price, treasury,
} convert { poa, vdf_limiter_info }
  convert_iter { system_ledgers, data_ledgers });

impl From<irys_types::IrysBlockHeader> for IrysBlockHeader {
    fn from(h: irys_types::IrysBlockHeader) -> Self {
        match h {
            irys_types::IrysBlockHeader::V1(inner) => Self::V1(inner.into()),
        }
    }
}

impl From<IrysBlockHeader> for irys_types::IrysBlockHeader {
    fn from(h: IrysBlockHeader) -> Self {
        match h {
            IrysBlockHeader::V1(inner) => Self::V1(inner.into()),
        }
    }
}

super::impl_mirror_from!(irys_types::BlockBody => BlockBody {
    block_hash,
} convert_iter { data_transactions, commitment_transactions });
