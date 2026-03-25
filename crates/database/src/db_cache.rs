use alloy_primitives::aliases::U232;
use arbitrary::Arbitrary;
use bytes::Buf as _;
use irys_types::{
    Base64, ChunkPathHash, Compact, H256, TxChunkOffset, UnixTimestamp, UnpackedChunk,
    partition::PartitionHash,
};
use reth_db::DatabaseError;
use reth_db::table::{Decode, Encode};
use serde::{Deserialize, Serialize};

// TODO: move all of these into types

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// partition hashes
/// TODO: use a custom Compact as the default for `Vec<T>` sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct PartitionHashes(pub Vec<PartitionHash>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
pub struct DataRootLRUEntry {
    /// The last block height this data_root was used
    pub last_height: u64,
}

// """constrained""" by PD: maximum addressable partitions: u200, with a u32 chunk offset
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize, Default, Arbitrary,
)]

pub struct GlobalChunkOffset(U232);

// 29 bytes, u232 -> 232 bits, 29 bytes
pub const GLOBAL_CHUNK_OFFSET_BYTES: usize = 29;

impl Encode for GlobalChunkOffset {
    type Encoded = [u8; GLOBAL_CHUNK_OFFSET_BYTES];

    fn encode(self) -> Self::Encoded {
        self.0.to_le_bytes()
    }
}
impl Decode for GlobalChunkOffset {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self(
            U232::try_from_le_slice(value).ok_or(DatabaseError::Decode)?,
        ))
    }
}

impl Compact for GlobalChunkOffset {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_slice(&self.0.to_le_bytes::<GLOBAL_CHUNK_OFFSET_BYTES>());
        GLOBAL_CHUNK_OFFSET_BYTES
    }

    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        let o = Self(U232::from_le_slice(buf));
        buf.advance(len);
        (o, buf)
    }
}
#[cfg(test)]
mod global_chunk_offset_tests {
    use super::*;
    use bytes::BytesMut;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn global_chunk_offset_compact_roundtrip(bytes in proptest::array::uniform29(0_u8..)) {
            let original = GlobalChunkOffset(U232::from_le_bytes(bytes));
            let mut buf = BytesMut::with_capacity(29 + 2);
            let len = original.to_compact(&mut buf);
            buf.extend_from_slice(&[0xAA, 0xBB]);
            let (decoded, rest) = GlobalChunkOffset::from_compact(&buf[..], len);
            prop_assert_eq!(decoded, original);
            prop_assert_eq!(rest, &[0xAA, 0xBB]);
        }
    }
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
pub struct CachedDataRoot {
    /// Total size (in bytes) of the data represented by the `data_root`
    pub data_size: u64,

    /// Has this data_size been confirmed by the data_path of the rightmost chunk in the data_root
    pub data_size_confirmed: bool,

    /// The set of all tx.ids' that contain this `data_root`
    pub txid_set: Vec<H256>,

    /// Block hashes for blocks containing transactions with this `data_root`
    pub block_set: Vec<H256>,

    /// Optional expiry height (e.g. anchor_height + anchor_expiry_depth) used for pruning while unconfirmed.
    /// If None, pruning falls back to block inclusion history.
    #[serde(default)]
    pub expiry_height: Option<u64>,

    /// Unix timestamp (seconds) when this data root was first cached
    /// Used for FIFO eviction strategies (both time-based and size-based)
    #[serde(default)]
    pub cached_at: UnixTimestamp,
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
pub struct CachedChunk {
    // optional as the chunk's data can be in a partition
    pub chunk: Option<Base64>,
    pub data_path: Base64,
}

impl From<UnpackedChunk> for CachedChunk {
    fn from(value: UnpackedChunk) -> Self {
        Self {
            chunk: Some(value.bytes),
            data_path: value.data_path,
        }
    }
}

// TODO: figure out if/how to use lifetimes to reduce the data cloning
// (the write to DB copies the bytes anyway so it should just need a ref)
impl From<&UnpackedChunk> for CachedChunk {
    fn from(value: &UnpackedChunk) -> Self {
        Self {
            chunk: Some(value.bytes.clone()),
            data_path: value.data_path.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct CachedChunkIndexEntry {
    pub index: TxChunkOffset, // subkey
    pub meta: CachedChunkIndexMetadata,
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
/// structure containing any chunk cache index metadata, like the `chunk_path_hash` for chunk data lookups
pub struct CachedChunkIndexMetadata {
    pub chunk_path_hash: ChunkPathHash,
    /// Last time this index entry was updated
    pub updated_at: UnixTimestamp,
}

impl From<CachedChunkIndexEntry> for CachedChunkIndexMetadata {
    fn from(value: CachedChunkIndexEntry) -> Self {
        value.meta
    }
}

/// note: the total size + the subkey must be < 2022 bytes (half a 4k DB page size - see MDBX .`set_geometry`)
const _: () = assert!(std::mem::size_of::<CachedChunkIndexEntry>() <= 2022);

// used for the Compact impl
const KEY_BYTES: usize = std::mem::size_of::<TxChunkOffset>();

// NOTE: Removing reth_codec and manually encode subkey
// and compress second part of the value. If we have compression
// over whole value (Even SubKey) that would mess up fetching of values with seek_by_key_subkey
// as the subkey ordering is byte ordering over the entire stored value, so the key 1.) has to be the first element that's encoded and 2.) cannot be compressed
impl Compact for CachedChunkIndexEntry {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // for now put full bytes and later compress it.
        // make sure your byte endianness is correct! for integers, it needs to be big endian so the ordering works correctly
        buf.put_slice(&self.index.to_be_bytes());
        let chunk_bytes = self.meta.to_compact(buf);
        chunk_bytes + KEY_BYTES
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let index = TxChunkOffset::from_be_bytes(buf[..KEY_BYTES].try_into().unwrap());
        let (meta, out) =
            CachedChunkIndexMetadata::from_compact(&buf[KEY_BYTES..], len - KEY_BYTES);
        (Self { index, meta }, out)
    }
}

/// converts a size (in bytes) to the number of chunks, rounding up (size 0 -> illegal state, size 1 -> 1, size 262144 -> 1, 262145 -> 2 )
pub fn data_size_to_chunk_count(data_size: u64, chunk_size: u64) -> eyre::Result<u32> {
    if data_size == 0 {
        return Err(eyre::eyre!("tx data_size 0 is illegal"));
    }
    Ok(data_size.div_ceil(chunk_size).try_into()?)
}

#[cfg(test)]
mod tests {
    use super::data_size_to_chunk_count;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn chunk_count_covers_data_size(
            data_size in 1_u64..=10_000_000,
            chunk_size in 1_u64..=262_144,
        ) {
            let count = data_size_to_chunk_count(data_size, chunk_size).unwrap();
            let count_u64 = u64::from(count);
            prop_assert!(
                count_u64 * chunk_size >= data_size,
                "chunk_count * chunk_size must cover data_size: {} * {} < {}",
                count, chunk_size, data_size
            );
        }

        #[test]
        fn chunk_count_is_minimal(
            data_size in 1_u64..=10_000_000,
            chunk_size in 1_u64..=262_144,
        ) {
            let count = data_size_to_chunk_count(data_size, chunk_size).unwrap();
            if count > 1 {
                let fewer = u64::from(count - 1);
                prop_assert!(
                    fewer * chunk_size < data_size,
                    "(count-1) * chunk_size must be insufficient: {} * {} >= {}",
                    count - 1, chunk_size, data_size
                );
            }
        }

        #[test]
        fn zero_data_size_is_error(chunk_size in 1_u64..=262_144) {
            prop_assert!(data_size_to_chunk_count(0, chunk_size).is_err());
        }

        #[test]
        fn exact_multiples_yield_exact_quotient(
            chunk_size in 1_u64..=262_144,
            multiplier in 1_u32..=100,
        ) {
            let data_size = chunk_size.saturating_mul(u64::from(multiplier));
            if data_size > 0 {
                let count = data_size_to_chunk_count(data_size, chunk_size).unwrap();
                prop_assert_eq!(count, multiplier);
            }
        }
    }
}
