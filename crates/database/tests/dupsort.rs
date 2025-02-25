use alloy_primitives::B256;
use arbitrary::Arbitrary;
use irys_types::Base64;
use reth_codecs::Compact;
use reth_db::cursor::DbCursorRO;
use reth_db::transaction::DbTxMut;
use reth_db::{table::DupSort, DatabaseError};
use reth_db::{tables, Database};
use reth_db::{HasName, HasTableType, TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::{fmt, u128};

use irys_database::{impl_compression_for_compact, open_or_create_db};
use reth_db::cursor::DbDupCursorRO;
use reth_db::transaction::DbTx;

use irys_database::db_cache::CachedChunk;

impl_compression_for_compact!(CachedChunk2);

tables! {
    DupSortTables;
    table CachedChunks2<Key = B256, Value = CachedChunk2, SubKey = u128>;
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct CachedChunk2 {
    pub key: u128,
    pub chunk: CachedChunk,
}

// NOTE: Removing reth_codec and manually encode subkey and compress second part of the value.
// If we have compression over whole value (Even SubKey) that would mess up fetching of values
// as the subkey ordering is byte ordering over the entire stored value.
// So the key 1.) has to be the first element that's encoded and 2.) cannot be compressed,
// and 3.) needs to be 'big endian' encoded (or a coding that works with the sorting)

const KEY_BYTES: usize = std::mem::size_of::<u128>();

impl Compact for CachedChunk2 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // for now put full bytes and later compress it.
        buf.put_slice(&self.key.to_be_bytes());
        self.chunk.to_compact(buf) + KEY_BYTES
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let key = u128::from_be_bytes(buf[..KEY_BYTES].try_into().unwrap());
        let (value, out) = CachedChunk::from_compact(&buf[KEY_BYTES..], len - KEY_BYTES);
        (Self { key, chunk: value }, out)
    }
}

/// `DupSort` works by allowing multiple unique values to be associated with a key,
/// with each duplicate value being sorted *by the encoded bytes of the value*;
/// Under BE encoding, subkey 0 would come before subkey 255.
/// This is why it's important the subkey is the first element that is encoded when serializing the value, as it defines the sort order for that encoded value.
/// Completely identical values (subkey + data) **are** deduplicated, but partially identical (same subkey + different data) are **NOT**, see chunk3.
#[test]
fn db_subkey_test() -> eyre::Result<()> {
    let builder = tempfile::Builder::new()
        .prefix("irys-test-")
        .rand_bytes(8)
        .tempdir();
    let tmpdir = builder
        .expect("Not able to create a temporary directory.")
        .into_path();

    let db = open_or_create_db(tmpdir, DupSortTables::ALL, None)?;
    let write_tx = db.tx_mut()?;
    // write two chunks to the same key
    let chunk = CachedChunk2 {
        key: u128::MIN,
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::default(),
        },
    };
    let key = B256::random();
    // complete duplicates are deduplicated, it means we don't need to check before inserting data that might already exist
    write_tx.put::<CachedChunks2>(key, chunk.clone())?;
    write_tx.put::<CachedChunks2>(key, chunk.clone())?;

    let chunk2 = CachedChunk2 {
        key: u128::MAX,
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::default(),
        },
    };
    write_tx.put::<CachedChunks2>(key, chunk2.clone())?;

    // important to note that we can have multiple unique values under the same subkey
    let chunk3 = CachedChunk2 {
        key: u128::MAX,
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::from_utf8_str("hello, world!")?,
        },
    };
    write_tx.put::<CachedChunks2>(key, chunk3.clone())?;
    write_tx.commit()?;

    // create a read cursor and walk the table, starting from a `None` subkey (so the entire subkey range)
    let mut dup_read_cursor = db.tx()?.cursor_dup_read::<CachedChunks2>()?;
    let walk = dup_read_cursor
        .walk_dup(Some(key), None)?
        .collect::<Result<Vec<_>, DatabaseError>>()?;

    // we should get all subkey'd chunks
    // note how the "smaller" subkey (repeat_byte(1)) is before the larger subkeys (repeat_byte(2))
    assert_eq!(
        walk,
        vec![(key, chunk.clone()), (key, chunk2), (key, chunk3)]
    );

    // index to a specific subkey value
    let seek_exact = dup_read_cursor.seek_by_key_subkey(key, chunk.key)?;

    assert_eq!(seek_exact, Some(chunk));

    // check the number of dupsort values associated with this key
    let n = dup_read_cursor.dup_count(key)?.unwrap();
    assert_eq!(n, 3);

    // delete the key, which also deletes all the associated duplicate values
    let w_tx = db.tx_mut()?;
    w_tx.delete::<CachedChunks2>(key, None)?;
    w_tx.commit()?;

    // new cursor - MDBX has isolation, so a new tx is required to "see" the changes
    let mut dup_read_cursor = db.tx()?.cursor_dup_read::<CachedChunks2>()?;

    let r = dup_read_cursor.seek_exact(key).unwrap();
    assert_eq!(r, None);

    let n2 = dup_read_cursor.dup_count(key)?;
    assert_eq!(n2, None);

    let walk = dup_read_cursor
        .walk_dup(Some(key), None)?
        .collect::<Result<Vec<_>, DatabaseError>>()?;

    assert_eq!(walk.len(), 0);

    Ok(())
}
