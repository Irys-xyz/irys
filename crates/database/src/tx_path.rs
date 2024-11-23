use crate::Ledger;
use bytes::{Buf, BufMut};
use irys_types::{BlockHash, BlockRelativeChunkOffset, Compact, TxPath, TxRoot};
use reth_db::table::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Compact, PartialOrd, Ord,
)]
/// This is a struct used to consistently construct the (primary) key used in the BlockRelativeTxPathIndex database
pub struct BlockRelativeTxPathIndexKey {
    /// the block hash of the block
    pub block_hash: BlockHash,
    /// the ledger number to scope the query to
    pub ledger: Ledger,
}

const BLOCK_TX_PATH_INDEX_KEY_SIZE: usize =
    std::mem::size_of::<BlockHash>() + std::mem::size_of::<Ledger>();

/// note: the total size + the subkey must be < 2022 bytes (half a 4k DB page size - see MDBX .set_geometry)
const _: () = assert!(BLOCK_TX_PATH_INDEX_KEY_SIZE <= 2022);

impl Encode for BlockRelativeTxPathIndexKey {
    type Encoded = [u8; BLOCK_TX_PATH_INDEX_KEY_SIZE];

    fn encode(self) -> Self::Encoded {
        let mut buf: Vec<u8> = Encode::encode(self.block_hash).into();
        buf.extend_from_slice(&self.ledger.get_id().to_be_bytes());
        buf.as_slice().try_into().unwrap() // todo: replace this?
    }
}

impl Decode for BlockRelativeTxPathIndexKey {
    fn decode(value: &[u8]) -> Result<Self, reth_db::DatabaseError> {
        let (block_hash, ledger_num) = value.split_last_chunk().unwrap();
        let block_hash = BlockHash::decode(block_hash)?;
        let ledger_num = u32::from_be_bytes(*ledger_num);
        Ok(Self {
            block_hash,
            ledger: *Ledger::ALL.get(ledger_num as usize).unwrap(), // TODO: probably a better way to do this?
        })
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq /* Compact */)]
/// This is a struct used to consistently construct the key used in the BlockRelativeTxPathIndex database
pub struct BlockRelativeTxPathIndexEntry {
    /// Subkey (offset) - this should be the offset of the last chunk associated with this Tx, as MDBX dupsort will return keys >=
    pub end_offset: BlockRelativeChunkOffset,
    /// the "value" being stored
    pub meta: BlockRelativeTxPathIndexMeta,
}

const KEY_BYTES: usize = std::mem::size_of::<BlockRelativeChunkOffset>();

// NOTE: Removing reth_codec and manually encode subkey
// and compress second part of the value. If we have compression
// over whole value (Even SubKey) that would mess up fetching of values with seek_by_key_subkey
// as the subkey ordering is byte ordering over the entire stored value, so the key 1.) has to be the first element that's encoded and 2.) cannot be compressed
impl Compact for BlockRelativeTxPathIndexEntry {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_slice(&self.end_offset.to_be_bytes());
        let path_bytes = self.meta.to_compact(buf);
        let total_bytes = path_bytes + KEY_BYTES;
        assert!(total_bytes < 2022);
        total_bytes
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let offset = BlockRelativeChunkOffset::from_be_bytes(buf[..KEY_BYTES].try_into().unwrap());
        let (meta, out) =
            BlockRelativeTxPathIndexMeta::from_compact(&buf[KEY_BYTES..], len - KEY_BYTES);
        (
            Self {
                end_offset: offset,
                meta,
            },
            out,
        )
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq /* Compact */)]
/// This is a struct used to contain any metadata that should be stored as part of the dupsort index entry
/// note: the total size + the subkey must be < 2022 bytes (half a 4k DB page size - see MDBX .set_geometry)

pub struct BlockRelativeTxPathIndexMeta {
    /// The transaction root associated with the block & ledger relative chunk offset
    pub tx_path: TxPath,
}

// custom Compact impl as the default impl for Vec<u8> sucks as it's for Vec<T>, and the specialized functions also suck
impl Compact for BlockRelativeTxPathIndexMeta {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let len = self.tx_path.len();
        buf.put_slice(&self.tx_path);
        len
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let mut v = Vec::with_capacity(len);
        v.put(buf.take(len));
        (BlockRelativeTxPathIndexMeta { tx_path: v }, buf)
    }
}

mod tests {
    use crate::{
        database, open_or_create_db,
        tables::{BlockRelativeTxPathIndex, Tables},
        tx_path::KEY_BYTES,
        Ledger,
    };
    use irys_testing_utils::utils::{setup_tracing_and_temp_dir, temporary_directory};
    use irys_types::{BlockHash, Compact, TxPath, TxRoot, H256};
    use rand::Rng as _;
    use reth::revm::primitives::Bytes;
    use reth_db::transaction::*;
    use reth_db::{cursor::*, DatabaseError};

    use reth_db::{
        table::{Decode, Encode},
        Database as _,
    };

    use super::{
        BlockRelativeTxPathIndexEntry, BlockRelativeTxPathIndexKey, BlockRelativeTxPathIndexMeta,
    };

    #[test]
    fn block_relative_tx_path_index_key_encode_decode() -> eyre::Result<()> {
        let key = BlockRelativeTxPathIndexKey {
            block_hash: BlockHash::random(),
            ledger: Ledger::Submit,
        };
        let mut buf = vec![];
        let enc_key_len = key.to_compact(&mut buf);
        let (key2, _) = BlockRelativeTxPathIndexKey::from_compact(&buf, enc_key_len);
        assert_eq!(key, key2);
        let enc_key = key.encode();
        let dec_key = BlockRelativeTxPathIndexKey::decode(&enc_key)?;
        assert_eq!(key, dec_key);
        Ok(())
    }

    #[test]
    fn block_relative_tx_path_index() -> eyre::Result<()> {
        let tmp_dir = setup_tracing_and_temp_dir(Some("block_relative_tx_path_index"), false);
        let db = open_or_create_db(tmp_dir, Tables::ALL, None)?;

        let data_size = 2000; // max key size is 2022
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let tx1_path = data_bytes.clone();
        let tx1_offset = 100;

        let tx2_path = data_bytes.clone()/* TxRoot::random() */;
        let tx2_offset = 142;

        // common block keys: hash & ledger
        let ledger = Ledger::Submit;
        let block_hash = BlockHash::zero();

        database::store_tx_path_by_block_offset(
            &db,
            block_hash,
            ledger,
            tx1_offset,
            tx1_path.clone(),
        )?;

        database::store_tx_path_by_block_offset(
            &db,
            block_hash,
            ledger,
            tx2_offset,
            tx2_path.clone(),
        )?;

        let tx_path =
            database::get_tx_path_by_block_ledger_offset(&db, block_hash, ledger, tx1_offset)?;
        assert_eq!(tx_path, Some(tx1_path.clone()));

        // get an offset before the tx1 end_offset, this should return tx1's tx_path
        let tx_path =
            database::get_tx_path_by_block_ledger_offset(&db, block_hash, ledger, tx1_offset - 10)?;
        assert_eq!(tx_path, Some(tx1_path.clone()));

        // get after tx1's end offset, this should return tx2's tx_path
        let tx_path =
            database::get_tx_path_by_block_ledger_offset(&db, block_hash, ledger, tx1_offset + 10)?;
        assert_eq!(tx_path, Some(tx2_path.clone()));

        // get exactly on tx2's end offset, should return tx2's data root
        let tx_path =
            database::get_tx_path_by_block_ledger_offset(&db, block_hash, ledger, tx2_offset)?;
        assert_eq!(tx_path, Some(tx2_path.clone()));

        // get beyond tx2's end offset, should return nothing.
        let tx_path =
            database::get_tx_path_by_block_ledger_offset(&db, block_hash, ledger, tx2_offset + 10)?;
        assert_eq!(tx_path, None);

        Ok(())
    }
}
