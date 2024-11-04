use serde::{Deserialize, Serialize};

use crate::{Base64, CHUNK_SIZE, H256};

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Chunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: H256,
    /// Total size of the data this chunk belongs to. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than CHUNK_SIZE.
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the data_root and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be CHUNK_SIZE in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    /// Offset of the chunk in the transactions data. Offsets are measured from
    /// the highest (last) byte in the chunk not the first.
    pub offset: usize,
}

pub type ChunkBin = [u8; CHUNK_SIZE as usize];
