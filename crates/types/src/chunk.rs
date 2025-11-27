use crate::IrysAddress;
use crate::{
    hash_sha256, partition::PartitionHash, string_u64, Base64, LedgerChunkOffset,
    PartitionChunkOffset, H256,
};
use arbitrary::Arbitrary;
use core::fmt;
use derive_more::{Add, From, Into};
use eyre::eyre;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::ops::{Add, Deref, DerefMut};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnpackedChunk {
    /// The root hash for this chunk which should map to the root_hash in the
    /// transaction header. Having this present makes it easier to do cached
    /// chunk lookups by data_root on the ingress node.
    pub data_root: DataRoot,
    /// Total size of the data stored by this data_root in bytes. Helps identify if this
    /// is the last chunk in the transactions data, or one that comes before it.
    /// Only the last chunk can be smaller than CHUNK_SIZE.
    #[serde(with = "string_u64")]
    pub data_size: u64,
    /// Raw bytes of the merkle proof that connects the data_root and the
    /// chunk hash
    pub data_path: Base64,
    /// Raw bytes to be stored, should be CHUNK_SIZE in length unless it is the
    /// last chunk in the transaction
    pub bytes: Base64,
    /// Index of the chunk in the transaction starting with 0
    pub tx_offset: TxChunkOffset,
}

impl UnpackedChunk {
    pub fn chunk_path_hash(&self) -> ChunkPathHash {
        Self::hash_data_path(&self.data_path.0)
    }

    pub fn hash_data_path(data_path: &ChunkDataPath) -> ChunkPathHash {
        hash_sha256(data_path).into()
    }

    /// Returns the transaction-relative byte offset of the last byte in this chunk.
    ///
    /// Chunk offsets in the merkle tree are based on end byte offsets:
    /// - Chunk 0: bytes [0, chunk_size)            → ends at chunk_size - 1
    /// - Chunk 1: bytes [chunk_size, 2*chunk_size) → ends at 2*chunk_size - 1
    /// - Last chunk may be partial                 → ends at data_size - 1
    ///
    /// Note: Converting from chunk count to chunk offset requires subtracting 1
    /// (e.g., 3 chunks means the last chunk has offset 2) the same is true
    /// of the byte offsets in the chunk.
    pub fn end_byte_offset(&self, chunk_size: u64) -> u64 {
        if self.data_size == 0 {
            return 0;
        }

        // Calculate total number of chunks, then calculate the last chunks offset
        let num_chunks = self.data_size.div_ceil(chunk_size);
        let last_chunk_offset = num_chunks - 1;

        if self.tx_offset.0 as u64 == last_chunk_offset {
            // If it's the last chunk, the end byte offset is just # of bytes - 1
            self.data_size - 1
        } else {
            // Intermediate chunks always end at a chunk boundary byte count minus 1 to make it an offset
            (self.tx_offset.0 as u64 + 1) * chunk_size - 1
        }
    }

    /// Returns the maximum valid tx_offset for this chunk's data_size.
    /// Returns None if data_size is 0.
    pub fn max_valid_offset(&self, chunk_size: u64) -> Option<u64> {
        if self.data_size == 0 {
            return None;
        }

        let num_chunks = self.data_size.div_ceil(chunk_size);
        Some(num_chunks.saturating_sub(1))
    }

    /// Validates that tx_offset is within valid bounds for the claimed data_size.
    ///
    /// Returns `true` if:
    /// - data_size > 0 AND tx_offset <= max_valid_offset
    /// - data_size == 0 AND tx_offset == 0
    ///
    /// Returns `false` otherwise (offset exceeds valid chunk count).
    pub fn is_valid_offset(&self, chunk_size: u64) -> bool {
        let offset_u64 = u64::from(self.tx_offset.0);

        match self.max_valid_offset(chunk_size) {
            Some(max_offset) => offset_u64 <= max_offset,
            None => offset_u64 == 0,
        }
    }

    /// Returns the end byte offset for this chunk, or None if calculation would overflow
    /// or if the offset is invalid for the data_size.
    pub fn end_byte_offset_checked(&self, chunk_size: u64) -> Option<u64> {
        if self.data_size == 0 {
            return Some(0);
        }

        let max_offset = self.max_valid_offset(chunk_size)?;
        let offset_u64 = u64::from(self.tx_offset.0);

        if offset_u64 > max_offset {
            return None;
        }

        if offset_u64 == max_offset {
            Some(self.data_size.saturating_sub(1))
        } else {
            offset_u64
                .checked_add(1)?
                .checked_mul(chunk_size)?
                .checked_sub(1)
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
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
    // #[serde(default, with = "address_base58_stringify")]
    pub packing_address: IrysAddress,
    /// the partition relative chunk offset
    pub partition_offset: PartitionChunkOffset,
    /// Index of the chunk in the transaction starting with 0
    pub tx_offset: TxChunkOffset,
    /// The hash of the partition containing this chunk
    pub partition_hash: PartitionHash,
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
    /// the partition relative chunk offset
    pub partition_relative_offset: Option<PartitionChunkOffset>,
    /// the Address used to pack this chunk
    // #[serde(with = "option_address_base58_stringify")]
    pub packing_address: Option<IrysAddress>,
    // Index of the chunk in the transaction starting with 0
    pub tx_offset: Option<TxChunkOffset>,
    /// The hash of the partition containing this chunk
    pub partition_hash: Option<PartitionHash>,
}

impl PartialChunk {
    /// Check if this partial chunk has enough fields populated to become an unpacked chunk
    pub fn is_full_unpacked_chunk(&self) -> bool {
        self.data_root.is_some()
            && self.data_size.is_some()
            && self.data_path.is_some()
            && self.bytes.is_some()
            && self.tx_offset.is_some()
    }

    pub fn is_full_packed_chunk(&self) -> bool {
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
            data_root: self.data_root.ok_or(err_fn("data_root"))?,
            data_size: self.data_size.ok_or(err_fn("data_size"))?,
            data_path: self.data_path.ok_or(err_fn("data_path"))?,
            bytes: self.bytes.ok_or(err_fn("bytes"))?,
            tx_offset: self.tx_offset.ok_or(err_fn("tx_offset"))?,
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
            tx_offset: self.tx_offset.ok_or(err_fn("tx_offset"))?,
            packing_address: self.packing_address.ok_or(err_fn("packing_address"))?,
            partition_offset: self
                .partition_relative_offset
                .ok_or(err_fn("partition_relative_offset"))?,
            partition_hash: self.partition_hash.ok_or(err_fn("partition_hash"))?,
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

/// An (unpacked) chunk's raw bytes
/// this type is unsized (i.e not a [u8; N]) as chunks can have variable sizes
/// either for testing or due to it being the last unpadded chunk
pub type ChunkBytes = Vec<u8>;

/// sha256(chunk_data_path)
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
    pub fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl Hash for TxChunkOffset {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
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
pub struct RelativeChunkOffset(pub i32);

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
    pub fn from_be_bytes(bytes: [u8; 4]) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    // Ensures zero-size data returns 0 and avoids underflow in end_byte_offset
    #[test]
    fn end_byte_offset_zero_size_returns_zero() {
        let chunk = UnpackedChunk {
            data_root: Default::default(),
            data_size: 0,
            data_path: Base64(Vec::new()),
            bytes: Base64(Vec::new()),
            tx_offset: TxChunkOffset(0),
        };
        assert_eq!(chunk.end_byte_offset(64), 0);
    }
    // Non-last chunk: end offset should be chunk_size - 1 for the first chunk
    #[test]
    fn end_byte_offset_full_chunk_non_last() {
        let chunk = UnpackedChunk {
            data_root: Default::default(),
            data_size: 200,
            data_path: Base64(Vec::new()),
            bytes: Base64(vec![0; 64]),
            tx_offset: TxChunkOffset(0),
        };
        assert_eq!(chunk.end_byte_offset(64), 64 - 1);
    }
    // Last (partial) chunk: end offset be one less than the total data_size (because it's an offset)
    #[test]
    fn end_byte_offset_last_chunk_trimmed() {
        let chunk = UnpackedChunk {
            data_root: Default::default(),
            data_size: 200,
            data_path: Base64(Vec::new()),
            bytes: Base64(vec![0; 8]),
            tx_offset: TxChunkOffset(3),
        };
        assert_eq!(chunk.end_byte_offset(64), 199);
    }
    // Last chunk exact multiple: end offset should be one less than the total data_size (because it's an offset)
    #[test]
    fn end_byte_offset_exact_multiple_last_full() {
        let chunk = UnpackedChunk {
            data_root: Default::default(),
            data_size: 128,
            data_path: Base64(Vec::new()),
            bytes: Base64(vec![0; 64]),
            tx_offset: TxChunkOffset(1),
        };
        assert_eq!(chunk.end_byte_offset(64), 127);
    }

    mod offset_validation {
        use super::*;
        use rstest::rstest;

        const CHUNK_SIZE: u64 = 256 * 1024;

        #[rstest]
        #[case(0, None)]
        #[case(1000, Some(0))]
        #[case(CHUNK_SIZE * 10, Some(9))]
        #[case(CHUNK_SIZE * 10 + CHUNK_SIZE / 2, Some(10))]
        fn max_valid_offset_cases(#[case] data_size: u64, #[case] expected: Option<u64>) {
            let chunk = UnpackedChunk {
                data_size,
                ..Default::default()
            };
            assert_eq!(chunk.max_valid_offset(CHUNK_SIZE), expected);
        }

        #[rstest]
        #[case(0, 0, true)]
        #[case(0, 1, false)]
        #[case(1000, 0, true)]
        #[case(1000, 1, false)]
        #[case(CHUNK_SIZE * 10, 0, true)]
        #[case(CHUNK_SIZE * 10, 9, true)]
        #[case(CHUNK_SIZE * 10, 10, false)]
        #[case(CHUNK_SIZE * 10, u32::MAX, false)]
        #[case(CHUNK_SIZE * 2 + CHUNK_SIZE / 2, 0, true)]
        #[case(CHUNK_SIZE * 2 + CHUNK_SIZE / 2, 2, true)]
        #[case(CHUNK_SIZE * 2 + CHUNK_SIZE / 2, 3, false)]
        fn is_valid_offset_cases(
            #[case] data_size: u64,
            #[case] tx_offset: u32,
            #[case] expected: bool,
        ) {
            let chunk = UnpackedChunk {
                data_size,
                tx_offset: TxChunkOffset(tx_offset),
                ..Default::default()
            };
            assert_eq!(chunk.is_valid_offset(CHUNK_SIZE), expected);
        }

        #[rstest]
        #[case(1000, 5, None)]
        #[case(CHUNK_SIZE * 3, 0, Some(CHUNK_SIZE - 1))]
        #[case(CHUNK_SIZE * 3, 2, Some(CHUNK_SIZE * 3 - 1))]
        #[case(0, 0, Some(0))]
        fn end_byte_offset_checked_cases(
            #[case] data_size: u64,
            #[case] tx_offset: u32,
            #[case] expected: Option<u64>,
        ) {
            let chunk = UnpackedChunk {
                data_size,
                tx_offset: TxChunkOffset(tx_offset),
                ..Default::default()
            };
            assert_eq!(chunk.end_byte_offset_checked(CHUNK_SIZE), expected);
        }
    }
}
