use crate::{CombinedBlockHeader, IrysTransactionHeader, UnpackedChunk};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    Block(CombinedBlockHeader),
}
