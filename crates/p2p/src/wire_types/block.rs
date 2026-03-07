use irys_types::{
    block::IrysTokenPrice,
    partition::PartitionHash,
    serialization::{Base64, H256List, IngressProofsList},
    BlockHash, IrysAddress, IrysSignature, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};

use super::{impl_version_tagged_serde, CommitmentTransaction, DataTransactionHeader};

// PoaData — plain struct, no versioning
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

// VDFLimiterInfo — plain struct
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemTransactionLedger {
    pub ledger_id: u32,
    pub tx_ids: H256List,
}

// IrysBlockHeaderV1 inner fields
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

/// Sovereign wire type for IrysBlockHeader (IntegerTagged-compatible).
#[derive(Debug, Clone, PartialEq)]
pub enum IrysBlockHeader {
    V1(IrysBlockHeaderV1Inner),
}

impl_version_tagged_serde!(IrysBlockHeader { 1 => V1(IrysBlockHeaderV1Inner) });

/// Sovereign wire type for BlockBody.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockBody {
    pub block_hash: BlockHash,
    pub data_transactions: Vec<DataTransactionHeader>,
    pub commitment_transactions: Vec<CommitmentTransaction>,
}

// -- Conversions --

impl From<&irys_types::PoaData> for PoaData {
    fn from(p: &irys_types::PoaData) -> Self {
        Self {
            partition_chunk_offset: p.partition_chunk_offset,
            partition_hash: p.partition_hash,
            chunk: p.chunk.clone(),
            ledger_id: p.ledger_id,
            tx_path: p.tx_path.clone(),
            data_path: p.data_path.clone(),
        }
    }
}

impl From<PoaData> for irys_types::PoaData {
    fn from(p: PoaData) -> Self {
        Self {
            partition_chunk_offset: p.partition_chunk_offset,
            partition_hash: p.partition_hash,
            chunk: p.chunk,
            ledger_id: p.ledger_id,
            tx_path: p.tx_path,
            data_path: p.data_path,
        }
    }
}

impl From<&irys_types::VDFLimiterInfo> for VDFLimiterInfo {
    fn from(v: &irys_types::VDFLimiterInfo) -> Self {
        Self {
            output: v.output,
            global_step_number: v.global_step_number,
            seed: v.seed,
            next_seed: v.next_seed,
            prev_output: v.prev_output,
            last_step_checkpoints: v.last_step_checkpoints.clone(),
            steps: v.steps.clone(),
            vdf_difficulty: v.vdf_difficulty,
            next_vdf_difficulty: v.next_vdf_difficulty,
        }
    }
}

impl From<VDFLimiterInfo> for irys_types::VDFLimiterInfo {
    fn from(v: VDFLimiterInfo) -> Self {
        Self {
            output: v.output,
            global_step_number: v.global_step_number,
            seed: v.seed,
            next_seed: v.next_seed,
            prev_output: v.prev_output,
            last_step_checkpoints: v.last_step_checkpoints,
            steps: v.steps,
            vdf_difficulty: v.vdf_difficulty,
            next_vdf_difficulty: v.next_vdf_difficulty,
        }
    }
}

impl From<&irys_types::DataTransactionLedger> for DataTransactionLedger {
    fn from(l: &irys_types::DataTransactionLedger) -> Self {
        Self {
            ledger_id: l.ledger_id,
            tx_root: l.tx_root,
            tx_ids: l.tx_ids.clone(),
            total_chunks: l.total_chunks,
            expires: l.expires,
            proofs: l.proofs.clone(),
            required_proof_count: l.required_proof_count,
        }
    }
}

impl From<DataTransactionLedger> for irys_types::DataTransactionLedger {
    fn from(l: DataTransactionLedger) -> Self {
        Self {
            ledger_id: l.ledger_id,
            tx_root: l.tx_root,
            tx_ids: l.tx_ids,
            total_chunks: l.total_chunks,
            expires: l.expires,
            proofs: l.proofs,
            required_proof_count: l.required_proof_count,
        }
    }
}

impl From<&irys_types::SystemTransactionLedger> for SystemTransactionLedger {
    fn from(l: &irys_types::SystemTransactionLedger) -> Self {
        Self {
            ledger_id: l.ledger_id,
            tx_ids: l.tx_ids.clone(),
        }
    }
}

impl From<SystemTransactionLedger> for irys_types::SystemTransactionLedger {
    fn from(l: SystemTransactionLedger) -> Self {
        Self {
            ledger_id: l.ledger_id,
            tx_ids: l.tx_ids,
        }
    }
}

impl From<&irys_types::IrysBlockHeader> for IrysBlockHeader {
    fn from(h: &irys_types::IrysBlockHeader) -> Self {
        match h {
            irys_types::IrysBlockHeader::V1(inner) => Self::V1(IrysBlockHeaderV1Inner {
                block_hash: inner.block_hash,
                signature: inner.signature,
                height: inner.height,
                diff: inner.diff,
                cumulative_diff: inner.cumulative_diff,
                solution_hash: inner.solution_hash,
                last_diff_timestamp: inner.last_diff_timestamp,
                previous_solution_hash: inner.previous_solution_hash,
                last_epoch_hash: inner.last_epoch_hash,
                chunk_hash: inner.chunk_hash,
                previous_block_hash: inner.previous_block_hash,
                previous_cumulative_diff: inner.previous_cumulative_diff,
                poa: (&inner.poa).into(),
                reward_address: inner.reward_address,
                reward_amount: inner.reward_amount,
                miner_address: inner.miner_address,
                timestamp: inner.timestamp,
                system_ledgers: inner.system_ledgers.iter().map(Into::into).collect(),
                data_ledgers: inner.data_ledgers.iter().map(Into::into).collect(),
                evm_block_hash: inner.evm_block_hash,
                vdf_limiter_info: (&inner.vdf_limiter_info).into(),
                oracle_irys_price: inner.oracle_irys_price,
                ema_irys_price: inner.ema_irys_price,
                treasury: inner.treasury,
            }),
        }
    }
}

impl TryFrom<IrysBlockHeader> for irys_types::IrysBlockHeader {
    type Error = eyre::Report;
    fn try_from(h: IrysBlockHeader) -> eyre::Result<Self> {
        match h {
            IrysBlockHeader::V1(inner) => Ok(Self::V1(irys_types::IrysBlockHeaderV1 {
                block_hash: inner.block_hash,
                signature: inner.signature,
                height: inner.height,
                diff: inner.diff,
                cumulative_diff: inner.cumulative_diff,
                solution_hash: inner.solution_hash,
                last_diff_timestamp: inner.last_diff_timestamp,
                previous_solution_hash: inner.previous_solution_hash,
                last_epoch_hash: inner.last_epoch_hash,
                chunk_hash: inner.chunk_hash,
                previous_block_hash: inner.previous_block_hash,
                previous_cumulative_diff: inner.previous_cumulative_diff,
                poa: inner.poa.into(),
                reward_address: inner.reward_address,
                reward_amount: inner.reward_amount,
                miner_address: inner.miner_address,
                timestamp: inner.timestamp,
                system_ledgers: inner.system_ledgers.into_iter().map(Into::into).collect(),
                data_ledgers: inner.data_ledgers.into_iter().map(Into::into).collect(),
                evm_block_hash: inner.evm_block_hash,
                vdf_limiter_info: inner.vdf_limiter_info.into(),
                oracle_irys_price: inner.oracle_irys_price,
                ema_irys_price: inner.ema_irys_price,
                treasury: inner.treasury,
            })),
        }
    }
}

impl From<&irys_types::BlockBody> for BlockBody {
    fn from(b: &irys_types::BlockBody) -> Self {
        Self {
            block_hash: b.block_hash,
            data_transactions: b.data_transactions.iter().map(Into::into).collect(),
            commitment_transactions: b.commitment_transactions.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<BlockBody> for irys_types::BlockBody {
    type Error = eyre::Report;
    fn try_from(b: BlockBody) -> eyre::Result<Self> {
        let data_transactions: Result<Vec<_>, _> = b
            .data_transactions
            .into_iter()
            .map(irys_types::DataTransactionHeader::try_from)
            .collect();
        let commitment_transactions: Result<Vec<_>, _> = b
            .commitment_transactions
            .into_iter()
            .map(irys_types::CommitmentTransaction::try_from)
            .collect();
        Ok(Self {
            block_hash: b.block_hash,
            data_transactions: data_transactions?,
            commitment_transactions: commitment_transactions?,
        })
    }
}
