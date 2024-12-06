use std::ops::Deref;

use arbitrary::Arbitrary;
use irys_types::{
    Base64, Chunk, ChunkPathHash, Compact, TxRelativeChunkIndex, TxRelativeChunkOffset, CHUNK_SIZE,
    H256,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
pub struct CachedDataRoot {
    /// Unlike a unix timestamp which stores the number of seconds since
    /// UNIX_EPOCH, this timestamp stores the number of milliseconds. Similar
    /// to javascript timestamps.
    pub timestamp: u128,

    /// Total size (in bytes) of the data represented by the data_root
    pub data_size: u64,

    /// The set of all tx.ids' that contain this data_root
    pub txid_set: Vec<H256>,
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
pub struct CachedChunk {
    // optional as the chunk's data can be in a partition
    pub chunk: Option<Base64>,
    pub data_path: Base64,
}

impl From<Chunk> for CachedChunk {
    fn from(value: Chunk) -> Self {
        Self {
            chunk: Some(value.bytes),
            data_path: value.data_path,
        }
    }
}

// TODO: figure out if/how to use lifetimes to reduce the data cloning
// (the write to DB copies the bytes anyway so it should just need a ref)
impl From<&Chunk> for CachedChunk {
    fn from(value: &Chunk) -> Self {
        Self {
            chunk: Some(value.bytes.clone()),
            data_path: value.data_path.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct CachedChunkIndexEntry {
    pub index: TxRelativeChunkIndex, // subkey
    pub meta: CachedChunkIndexMetadata,
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary, Compact)]
/// structure containing any chunk cache index metadata, like the chunk_path_hash for chunk data lookups
pub struct CachedChunkIndexMetadata {
    pub chunk_path_hash: ChunkPathHash,
}

impl From<CachedChunkIndexEntry> for CachedChunkIndexMetadata {
    fn from(value: CachedChunkIndexEntry) -> Self {
        value.meta
    }
}

/// note: the total size + the subkey must be < 2022 bytes (half a 4k DB page size - see MDBX .set_geometry)
const _: () = assert!(std::mem::size_of::<CachedChunkIndexEntry>() <= 2022);

// used for the Compact impl
const KEY_BYTES: usize = std::mem::size_of::<TxRelativeChunkIndex>();

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
        // make sure your byte endianess is correct! for integers, it needs to be big endian so the ordering works correctly
        buf.put_slice(&self.index.to_be_bytes());
        let chunk_bytes = self.meta.to_compact(buf);
        chunk_bytes + KEY_BYTES
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let index = TxRelativeChunkIndex::from_be_bytes(buf[..KEY_BYTES].try_into().unwrap());
        let (meta, out) =
            CachedChunkIndexMetadata::from_compact(&buf[KEY_BYTES..], len - KEY_BYTES);
        (Self { index, meta }, out)
    }
}
/// convert a chunk's tx relative offset to a tx relative index (i.e offset 262144 -> index 0, offset 262145 -> index 1)
/// due to the fact offsets are the end bound, we minus 1 to get the intuitive 0 indexed offsets
pub fn chunk_offset_to_index(
    offset: TxRelativeChunkOffset,
    chunk_size: u64,
) -> eyre::Result<TxRelativeChunkIndex> {
    let div: u32 = offset.div_ceil(chunk_size.try_into()?).try_into()?;
    Ok(div - 1)
}

/// converts a size (in bytes) to the number of chunks, rounding up (size 0 -> illegal state, size 1 -> 1, size 262144 -> 1, 262145 -> 2 )
pub fn data_size_to_chunk_count(data_size: u64, chunk_size: u64) -> eyre::Result<u32> {
    assert_ne!(data_size, 0, "tx data_size 0 is illegal");
    Ok(data_size.div_ceil(chunk_size.try_into()?).try_into()?)
}
