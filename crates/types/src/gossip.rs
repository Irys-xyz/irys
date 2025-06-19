use crate::{CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};
use alloy_primitives::Address;
use base58::ToBase58 as _;
use reth::rpc::types::engine::{ExecutionData, ExecutionPayload};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(ExecutionData),
}

impl GossipData {
    pub fn data_type_and_id(&self) -> String {
        match self {
            Self::Chunk(chunk) => {
                format!("chunk data root {}", chunk.data_root)
            }
            Self::Transaction(tx) => {
                format!("transaction {}", tx.id.0.to_base58())
            }
            Self::CommitmentTransaction(commitment_tx) => {
                format!("commitment transaction {}", commitment_tx.id.0.to_base58())
            }
            Self::Block(block) => {
                format!("block {} height: {}", block.block_hash, block.height)
            }
            Self::ExecutionPayload(execution_payload) => {
                format!(
                    "execution payload for block {}",
                    execution_payload.payload.as_v1().block_hash
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequest<T> {
    pub miner_address: Address,
    pub data: T,
}
