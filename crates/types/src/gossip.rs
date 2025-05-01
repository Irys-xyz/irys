use crate::{IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};
use base58::ToBase58;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
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
            GossipData::Block(block) => {
                format!("block {}", block.block_hash.0.to_base58())
            }
        }
    }
}
