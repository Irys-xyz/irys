use core::fmt;
use std::ops::{Add, Deref, DerefMut};

use crate::{string_u64, LedgerChunkOffset};
use alloy_primitives::Address;
use arbitrary::Arbitrary;
use derive_more::{Add, From, Into};
use eyre::eyre;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

use crate::{hash_sha256, partition::PartitionHash, Base64, PartitionChunkOffset, H256};

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
// tag is to produce better JSON serialization, it flattens { "Packed": {...}} to {type: "packed", ... }
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChunkFormat {
    Unpacked(UnpackedChunk),
    Packed(PackedChunk),
}

impl ChunkFormat {
    pub fn as_packed(self) -> Option<PackedChunk> {
        match self {
            Self::Unpacked(_) => None,
            Self::Packed(packed_chunk) => Some(packed_chunk),
        }
    }

    pub fn as_unpacked(self) -> Option<UnpackedChunk> {
        match self {
            Self::Unpacked(unpacked_chunk) => Some(unpacked_chunk),
            Self::Packed(_) => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnpackedChunk {
    /// The root hash for this chunk which should map to the `root_hash` in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by `data_root` on the ingress node.
    pub data_root: DataRoot,
    /// Total size of the data stored by this `data_root` in bytes. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than `CHUNK_SIZE`.
    #[serde(with = "string_u64")]
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the `data_root` and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be `CHUNK_SIZE` in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    // Index of the chunk in the transaction starting with 0
    pub tx_offset: TxChunkOffset,
}

impl UnpackedChunk {
    pub fn chunk_path_hash(&self) -> ChunkPathHash {
        Self::hash_data_path(&self.data_path.0)
    }

    pub fn hash_data_path(data_path: &ChunkDataPath) -> ChunkPathHash {
        hash_sha256(data_path).unwrap().into()
    }

    /// a Chunk's tx relative byte offset
    /// due to legacy weirdness, the offset is of the end of the chunk, not the start
    /// i.e for the first chunk, the offset is `chunk_size` instead of 0
    pub fn byte_offset(&self, chunk_size: u64) -> u64 {
        let last_index = self.data_size.div_ceil(chunk_size);
        if self.tx_offset.0 as u64 == last_index {
            self.data_size
        } else {
            (*self.tx_offset + 1) as u64 * chunk_size - 1
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PackedChunk {
    /// The root hash for this chunk which should map to the `root_hash` in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by `data_root` on the ingress node.
    pub data_root: DataRoot,
    /// Total size of the data stored by this `data_root`. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than `CHUNK_SIZE`.
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the `data_root` and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be `CHUNK_SIZE` in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    /// the Address used to pack this chunk
    pub packing_address: Address,
    /// the partition relative chunk offset
    pub partition_offset: PartitionChunkOffset,
    /// Index of the chunk in the transaction starting with 0
    pub tx_offset: TxChunkOffset,
    /// The hash of the partition containing this chunk
    pub partition_hash: PartitionHash,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, Eq, PartialEq)]
/// A "partial chunk" that allows you to build up a chunk piece by piece
/// type alignment with the Packed and Unpacked chunk structs is enforced by the `TryInto` methods
pub struct PartialChunk {
    /// The root hash for this chunk which should map to the `root_hash` in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by `data_root` on the ingress node.
    pub data_root: Option<DataRoot>,
    /// Total size of the data stored by this `data_root`. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than `CHUNK_SIZE`.
    pub data_size: Option<u64>,
    /// Raw bytes of the merkle proof that connects the `data_root` and the
    /// chunk hash
    pub data_path: Option<Base64>,
    /// Raw bytes to be stored, should be `CHUNK_SIZE` in length unless it is the
    /// last chunk in the transaction
    pub bytes: Option<Base64>,
    /// the partition relative chunk offset
    pub partition_relative_offset: Option<PartitionChunkOffset>,
    /// the Address used to pack this chunk
    pub packing_address: Option<Address>,
    // Index of the chunk in the transaction starting with 0
    pub tx_offset: Option<TxChunkOffset>,
    /// The hash of the partition containing this chunk
    pub partition_hash: Option<PartitionHash>,
}

impl PartialChunk {
    /// Check if this partial chunk has enough fields populated to become an unpacked chunk
    pub const fn is_full_unpacked_chunk(&self) -> bool {
        self.data_root.is_some()
            && self.data_size.is_some()
            && self.data_path.is_some()
            && self.bytes.is_some()
            && self.tx_offset.is_some()
    }

    pub const fn is_full_packed_chunk(&self) -> bool {
        self.is_full_unpacked_chunk()
            && self.packing_address.is_some()
            && self.partition_relative_offset.is_some()
            && self.partition_hash.is_some()
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
            data_root: self.data_root.ok_or_else(||err_fn("data_root"))?,
            data_size: self.data_size.ok_or_else(||err_fn("data_size"))?,
            data_path: self.data_path.ok_or_else(||err_fn("data_path"))?,
            bytes: self.bytes.ok_or_else(||err_fn("bytes"))?,
            tx_offset: self.tx_offset.ok_or_else(||err_fn("tx_offset"))?,
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
            data_root: self.data_root.ok_or_else(||err_fn("data_root"))?,
            data_size: self.data_size.ok_or_else(||err_fn("data_size"))?,
            data_path: self.data_path.ok_or_else(||err_fn("data_path"))?,
            bytes: self.bytes.ok_or_else(||err_fn("bytes"))?,
            tx_offset: self.tx_offset.ok_or_else(||err_fn("tx_offset"))?,
            packing_address: self.packing_address.ok_or_else(||err_fn("packing_address"))?,
            partition_offset: self
                .partition_relative_offset
                .ok_or_else(||err_fn("partition_relative_offset"))?,
            partition_hash: self.partition_hash.ok_or_else(||err_fn("partition_hash"))?,
        })
    }
}

// #[test]
// fn chunk_json() {
//     let chunk = PackedChunk::default();
//     println!("{:?}", serde_json::to_string_pretty(&chunk));
//     let wrapped = ChunkFormat::Packed(chunk);
//     println!("{:?}", serde_json::to_string_pretty(&wrapped));
// }

/// a chunk binary
/// this type is unsized (i.e not a [u8; N]) as chunks can have variable sizes
/// either for testing or due to it being the last unpadded chunk
pub type ChunkBytes = Vec<u8>;

/// `sha256`(`chunk_data_path`)
pub type ChunkPathHash = H256;

// the root node ID for the merkle tree containing all the transaction's chunks
pub type DataRoot = H256;

/// The offset of the chunk relative to the first (0th) chunk of the data tree
#[derive(
    Default,
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Arbitrary,
    Add,
    From,
    Into,
)]
pub struct TxChunkOffset(pub u32);
impl TxChunkOffset {
    pub const fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl Deref for TxChunkOffset {
    type Target = u32;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for TxChunkOffset {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<LedgerChunkOffset> for TxChunkOffset {
    fn from(ledger: LedgerChunkOffset) -> Self {
        Self::from(*ledger)
    }
}

impl From<u64> for TxChunkOffset {
    fn from(value: u64) -> Self {
        Self(value as u32)
    }
}
impl From<i32> for TxChunkOffset {
    fn from(value: i32) -> Self {
        Self(value.try_into().unwrap())
    }
}
impl From<RelativeChunkOffset> for TxChunkOffset {
    fn from(value: RelativeChunkOffset) -> Self {
        Self::from(*value)
    }
}

impl fmt::Display for TxChunkOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// the Block relative chunk offset
#[derive(
    Default, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, From, Into,
)]
pub struct BlockChunkOffset(u64);

/// Used to track chunk offset ranges that span storage modules
///  a negative offset means the range began in a prior partition/storage module
#[derive(
    Default,
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Compact,
    Add,
    From,
    Into,
)]
pub struct RelativeChunkOffset(i32);

impl Deref for RelativeChunkOffset {
    type Target = i32;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for RelativeChunkOffset {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl RelativeChunkOffset {
    pub const fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(i32::from_be_bytes(bytes))
    }
}

impl Add<i32> for RelativeChunkOffset {
    type Output = Self;

    fn add(self, rhs: i32) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl fmt::Display for RelativeChunkOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl TryFrom<RelativeChunkOffset> for u32 {
    type Error = &'static str;

    fn try_from(offset: RelativeChunkOffset) -> Result<Self, Self::Error> {
        if offset.0 >= 0 {
            Ok(offset.0 as Self)
        } else {
            Err("Cannot convert negative RelativeChunkOffset to u32")
        }
    }
}

/// A chunks's data path
pub type ChunkDataPath = Vec<u8>;
