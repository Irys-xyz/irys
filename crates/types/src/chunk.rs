use serde::{Deserialize, Serialize};

use crate::{Base64, CHUNK_SIZE, H256};

#[derive(Serialize, Deserialize, Debug, Default, PartialEq)]
pub struct Chunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: DataRoot,
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
    pub offset: TxRelativeChunkOffset,
}

/// a Chunk's tx relative offset
/// due to legacy weirdness, the offset is of the end of the chunk, not the start
/// i.e for the first chunk, the offset is 262144 instead of 0
pub type TxRelativeChunkOffset = u32;

/// a fully padded chunk binary - note: only use this type in cases where smaller/end chunks *should* be padded to fill out the chunk
pub type ChunkBin = [u8; CHUNK_SIZE as usize];

/// a chunk binary - use this in cases where chunks may not be padded
pub type ChunkBytes = Vec<u8>;

/// sha256(chunk_path)
pub type ChunkPathHash = H256;

// the root node ID for the merkle tree containing all the transaction's chunks
pub type DataRoot = H256;

/// The 0-indexed index of the chunk relative to the first chunk of the tx's data tree
pub type TxRelativeChunkIndex = u32;

pub type DataChunks = Vec<Vec<u8>>;

pub type ChunkOffset = u32;

/// A transaction's data path
pub type DataPath = Vec<u8>;
