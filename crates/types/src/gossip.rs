use crate::{CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};
use alloy_primitives::Address;
use base58::ToBase58;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
}

impl GossipData {
    pub fn data_type_and_id(&self) -> String {
        match self {
            GossipData::Chunk(chunk) => {
                format!("chunk data root {}", chunk.data_root)
            }
            GossipData::Transaction(tx) => {
                format!("transaction {}", tx.id.0.to_base58())
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                format!("commitment transaction {}", commitment_tx.id.0.to_base58())
            }
            GossipData::Block(block) => {
                format!("block {}", block.block_hash.0.to_base58())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequest<T> {
    pub miner_address: Address,
    pub data: T,
}
