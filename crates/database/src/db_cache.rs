use alloy_primitives::aliases::U232;
use arbitrary::Arbitrary;
use bytes::Buf as _;
use irys_types::{
    partition::PartitionHash, Base64, ChunkPathHash, Compact, TxChunkOffset, UnpackedChunk, H256,
};
use reth_db::table::{Decode, Encode};
use reth_db::DatabaseError;
use serde::{Deserialize, Serialize};

// TODO: move all of these into types

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// partition hashes
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct PartitionHashes(pub Vec<PartitionHash>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
pub struct DataRootLRUEntry {
    /// The last block height this data_root was used
    pub last_height: u64,
    pub ingress_proof: bool, // TODO: use bitflags
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize, Default, Arbitrary,
)]

// """constrained""" by PD: maximum addressible partitions: u200, with a u32 chunk offset
pub struct GlobalChunkOffset(U232);

impl Encode for GlobalChunkOffset {
    type Encoded = [u8; 29];

    fn encode(self) -> Self::Encoded {
        self.0.to_le_bytes()
    }
}
impl Decode for GlobalChunkOffset {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(GlobalChunkOffset(
            U232::try_from_le_slice(value).ok_or(DatabaseError::Decode)?,
        ))
    }
}

impl Compact for GlobalChunkOffset {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_slice(&self.0.to_le_bytes::<29>());
        29
    }

    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        let o = GlobalChunkOffset(U232::from_le_slice(buf));
        buf.advance(len);
        (o, buf)
    }
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
pub struct CachedDataRoot {
    /// Unlike a unix timestamp which stores the number of seconds since
    /// `UNIX_EPOCH`, this timestamp stores the number of milliseconds. Similar
    /// to javascript timestamps.
    pub timestamp: u128,

    /// Total size (in bytes) of the data represented by the `data_root`
    pub data_size: u64,

    /// The set of all tx.ids' that contain this `data_root`
    pub txid_set: Vec<H256>,
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
    assert_ne!(data_size, 0, "tx data_size 0 is illegal");
    Ok(data_size.div_ceil(chunk_size).try_into()?)
}
