use serde::{Deserialize, Serialize};
use crate::{CombinedBlockHeader, IrysTransactionHeader, UnpackedChunk};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    Block(CombinedBlockHeader),
}