use arbitrary::Arbitrary;
use irys_types::Base64;
use reth::revm::primitives::B256;
use reth_codecs::Compact;
use reth_db::transaction::DbTxMut;
use reth_db::{table::DupSort, DatabaseError};
use reth_db::{tables, Database};
use reth_db::{HasName, HasTableType, TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::fmt;

use database::{impl_compression_for_compact, open_or_create_db};
use reth_db::cursor::DbDupCursorRO;
use reth_db::transaction::DbTx;

use database::db_cache::CachedChunk;

impl_compression_for_compact!(CachedChunk2);

tables! {
    table CachedChunks2<Key = B256, Value = CachedChunk2, SubKey = B256>;
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Serialize, Deserialize, Arbitrary)]
pub struct CachedChunk2 {
    pub key: B256,
    pub chunk: CachedChunk,
}

// NOTE: Removing reth_codec and manually encode subkey
// and compress second part of the value. If we have compression
// over whole value (Even SubKey) that would mess up fetching of values with seek_by_key_subkey
// as the subkey ordering is byte ordering over the entire stored value, so the key 1.) has to be the first element that's encoded and 2.) cannot be compressed
impl Compact for CachedChunk2 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // for now put full bytes and later compress it.
        buf.put_slice(&self.key[..]);
        self.chunk.to_compact(buf) + 32
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let key = B256::from_slice(&buf[..32]);
        let (value, out) = CachedChunk::from_compact(&buf[32..], len - 32);
        (Self { key, chunk: value }, out)
    }
}

/// DupSort works by allowing multiple unique values to be associated with a key,
/// with each value being sorted *by the encoded bytes of the value*, 0 being first, 255 being last.
/// this is why it's important the subkey is the first element that is encoded when serializing the value, as it defines the sort order for that encoded value
/// completely identical values (subkey + data) **are** deduplicated, but partially indentical (same subkey + different data) are **NOT**, see chunk3
#[test]
fn db_subkey_test() -> eyre::Result<()> {
    let builder = tempfile::Builder::new()
        .prefix("reth-test-")
        .rand_bytes(8)
        .tempdir();
    let tmpdir = builder
        .expect("Not able to create a temporary directory.")
        .into_path();

    let db = open_or_create_db(tmpdir)?.with_metrics_and_tables(Tables::ALL);
    let tx = db.tx_mut()?;
    // write two chunks to the same key
    let chunk = CachedChunk2 {
        key: B256::repeat_byte(1),
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::default(),
        },
    };
    let key = B256::random();
    // complete duplicates are deduplicated, it means we don't need to check before inserting data that might already exist
    tx.put::<CachedChunks2>(key, chunk.clone())?;
    tx.put::<CachedChunks2>(key, chunk.clone())?;

    let chunk2 = CachedChunk2 {
        key: B256::repeat_byte(2),
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::default(),
        },
    };
    tx.put::<CachedChunks2>(key, chunk2.clone())?;

    // important to note that we can have multiple unique values under the same subkey
    let chunk3 = CachedChunk2 {
        key: B256::repeat_byte(2),
        chunk: CachedChunk {
            chunk: None,
            data_path: Base64::from_utf8_str("hello, world!")?,
        },
    };
    tx.put::<CachedChunks2>(key, chunk3.clone())?;
    tx.commit()?;

    // create a read cursor and walk the table, starting from a `None` subkey (so the entire subkey range)
    let mut c = db.tx()?.cursor_dup_read::<CachedChunks2>()?;
    let res = c
        .walk_dup(Some(key), None)?
        .collect::<Result<Vec<_>, DatabaseError>>()?;

    // we should get all subkey'd chunks
    // note how the "smaller" subkey (repeat_byte(1)) is before the larger subkeys (repeat_byte(2))
    assert_eq!(
        res,
        vec![(key, chunk.clone()), (key, chunk2), (key, chunk3)]
    );

    // index to a specific subkey value
    let r = db
        .tx()?
        .cursor_dup_read::<CachedChunks2>()?
        .seek_by_key_subkey(key, chunk.key)?;

    assert_eq!(r, Some(chunk));

    Ok(())
}
