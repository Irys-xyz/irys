use alloy_primitives::Address;
use eyre::eyre;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

use crate::{hash_sha256, Base64, H256};

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct UnpackedChunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: DataRoot,
    /// Total size of the data stored by this data_root. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than CHUNK_SIZE.
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the data_root and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be CHUNK_SIZE in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    // Index of the chunk in the transaction starting with 0
    pub chunk_index: TxRelativeChunkIndex,
}

impl UnpackedChunk {
    pub fn chunk_path_hash(&self) -> ChunkPathHash {
        UnpackedChunk::hash_data_path(&self.data_path.0)
    }

    pub fn hash_data_path(data_path: &ChunkDataPath) -> ChunkPathHash {
        hash_sha256(data_path).unwrap().into()
    }

    /// a Chunk's tx relative byte offset
    /// due to legacy weirdness, the offset is of the end of the chunk, not the start
    /// i.e for the first chunk, the offset is chunk_size instead of 0
    pub fn byte_offset(&self, chunk_size: u64) -> u64 {
        let last_index = self.data_size.div_ceil(chunk_size as u64);
        if self.chunk_index as u64 == last_index {
            return self.data_size;
        } else {
            return (self.chunk_index + 1) as u64 * chunk_size - 1;
        }
    }
}

// This is mostly an expansion of the `Debug` derive
// TODO: use field_with to remove the extraneous quotation marks around the truncated bytes formatting
impl Display for UnpackedChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes,
            chunk_index,
        } = self;
        f.debug_struct("PackedChunk")
            .field("data_root", &data_root)
            .field("data_size", &data_size)
            .field("data_path", &truncate_format_bytes_hex(&data_path.0, 4))
            .field("bytes", &truncate_format_bytes_hex(&bytes.0, 4))
            .field("chunk_index", &chunk_index)
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ChunkEnum {
    Unpacked(UnpackedChunk),
    Packed(PackedChunk),
}

impl Display for ChunkEnum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkEnum::Unpacked(unpacked_chunk) => unpacked_chunk.fmt(f),
            ChunkEnum::Packed(packed_chunk) => packed_chunk.fmt(f),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PackedChunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: DataRoot,
    /// Total size of the data stored by this data_root. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than CHUNK_SIZE.
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the data_root and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be CHUNK_SIZE in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    /// the Address used to pack this chunk
    pub packing_address: Address,
    // Index of the chunk in the transaction starting with 0
    pub chunk_index: TxRelativeChunkIndex,
}

impl Display for PackedChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let PackedChunk {
            data_root,
            data_size,
            data_path,
            bytes,
            packing_address,
            chunk_index,
        } = self;
        f.debug_struct("PackedChunk")
            .field("data_root", &data_root)
            .field("data_size", &data_size)
            .field("data_path", &truncate_format_bytes_hex(&data_path.0, 4))
            .field("bytes", &truncate_format_bytes_hex(&bytes.0, 4))
            .field("packing_address", &packing_address)
            .field("chunk_index", &chunk_index)
            .finish()
    }
}

// formats a &[u8] into a hex encoded string, formatting it as <n bytes>...<n bytes>, or as just `<n bytes>` if the length < 2n
fn truncate_format_bytes_hex(bytes: &[u8], n: usize) -> String {
    let fmt = |b: &[u8]| b.iter().map(|b| format!("{:02x}", b)).collect::<String>();
    if bytes.len() <= 2 * n {
        fmt(bytes)
    } else {
        format!("{}...{}", fmt(&bytes[..n]), fmt(&bytes[bytes.len() - n..]))
    }
}

// formats a &[u8] into a  string, formatting it as <n bytes>...<n bytes>, or as just `<n bytes>` if the length < 2n
fn truncate_format_bytes(bytes: &[u8], n: usize) -> String {
    let fmt = |b: &[u8]| {
        b.iter()
            .map(|b| format!("{}", b))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if bytes.len() <= 2 * n {
        fmt(bytes)
    } else {
        format!("{}...{}", fmt(&bytes[..n]), fmt(&bytes[bytes.len() - n..]))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
/// A "partial chunk" that allows you to build up a chunk piece by piece
/// type alignment with the Packed and Unpacked chunk structs is enforced by the TryInto methods
pub struct PartialChunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: Option<DataRoot>,
    /// Total size of the data stored by this data_root. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than CHUNK_SIZE.
    pub data_size: Option<u64>,
    /// Raw bytes of the merkle proof that connects the data_root and the
    /// chunk hash
    pub data_path: Option<Base64>,
    /// Raw bytes to be stored, should be CHUNK_SIZE in length unless it is the
    /// last chunk in the transaction
    pub bytes: Option<Base64>,
    /// the Address used to pack this chunk
    pub packing_address: Option<Address>,
    // Index of the chunk in the transaction starting with 0
    pub chunk_index: Option<TxRelativeChunkIndex>,
}

impl PartialChunk {
    /// Check if this partial chunk has enough fields populated to become an unpacked chunk
    pub fn is_full_unpacked_chunk(&self) -> bool {
        self.data_root.is_some()
            && self.data_size.is_some()
            && self.data_path.is_some()
            && self.bytes.is_some()
            && self.chunk_index.is_some()
    }

    pub fn is_full_packed_chunk(&self) -> bool {
        self.is_full_unpacked_chunk() && self.packing_address.is_some()
    }
}

impl TryInto<UnpackedChunk> for PartialChunk {
    type Error = eyre::Error;

    fn try_into(self) -> Result<UnpackedChunk, Self::Error> {
        let err_fn = |s: &str| {
            eyre!(
                "Partial chunk is missing required field {} for UnpackedChunk",
                s
            )
        };

        Ok(UnpackedChunk {
            data_root: self.data_root.ok_or(err_fn("data_root"))?,
            data_size: self.data_size.ok_or(err_fn("data_size"))?,
            data_path: self.data_path.ok_or(err_fn("data_path"))?,
            bytes: self.bytes.ok_or(err_fn("bytes"))?,
            chunk_index: self.chunk_index.ok_or(err_fn("chunk_index"))?,
        })
    }
}

impl TryInto<PackedChunk> for PartialChunk {
    type Error = eyre::Error;

    fn try_into(self) -> Result<PackedChunk, Self::Error> {
        let err_fn = |s: &str| {
            eyre!(
                "Partial chunk is missing required field {} for PackedChunk",
                s
            )
        };

        Ok(PackedChunk {
            data_root: self.data_root.ok_or(err_fn("data_root"))?,
            data_size: self.data_size.ok_or(err_fn("data_size"))?,
            data_path: self.data_path.ok_or(err_fn("data_path"))?,
            bytes: self.bytes.ok_or(err_fn("bytes"))?,
            chunk_index: self.chunk_index.ok_or(err_fn("chunk_index"))?,
            packing_address: self.packing_address.ok_or(err_fn("packing_address"))?,
        })
    }
}

// }

/// a chunk binary
/// this type is unsized (i.e not a [u8; N]) as chunks can have variable sizes
/// either for testing or due to it being the last unpadded chunk
pub type ChunkBytes = Vec<u8>;

/// sha256(chunk_data_path)
pub type ChunkPathHash = H256;

// the root node ID for the merkle tree containing all the transaction's chunks
pub type DataRoot = H256;

/// The 0-indexed index of the chunk relative to the first chunk of the tx's data tree
pub type TxRelativeChunkIndex = u32;

pub type DataChunks = Vec<Vec<u8>>;

/// the Block relative chunk offset
pub type BlockRelativeChunkOffset = u64;

/// Used to track chunk offset ranges that span storage modules
///  a negative offset means the range began in a prior partition/storage module
pub type RelativeChunkOffset = i32;

/// A chunks's data path
pub type ChunkDataPath = Vec<u8>;
