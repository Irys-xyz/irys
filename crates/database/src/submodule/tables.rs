use irys_types::{
    ingress::IngressProof, ChunkDataPath, ChunkOffset, ChunkPathHash, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, RelativeChunkOffset, TxPath, TxPathHash, TxRelativeChunkIndex, H256,
};
use reth_codecs::Compact;
use reth_db::{
    table::{DupSort, Table},
    tables, Database, DatabaseError,
};
use reth_db::{HasName, HasTableType, TableType, TableViewer};
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{
    db_cache::{CachedChunk, CachedChunkIndexEntry, CachedDataRoot},
    open_or_create_db,
};

/// Per-submodule database tables
tables! {
    SubmoduleTables;

    /// Maps a partition relative offset to a chunk's path hashes
    /// note: mdbx keys are always sorted, so range queries work :)
    /// TODO: use custom Compact impl for Vec<u8> so we don't have problems
    table ChunkPathHashByOffset<Key = ChunkOffset, Value = ChunkPathHashes>;

    /// Maps a chunk's data path hash to the full data path
    table ChunkDataPathByPathHash<Key = ChunkPathHash, Value = ChunkDataPath>;

    /// Maps a tx path hash to the full tx path
    table TxPathByTxPathHash<Key = TxPathHash, Value = TxPath>;

    /// Maps a chunk path hash to the list of submodule-relative offsets it should inhabit
    table ChunkOffsetsByPathHash<Key = ChunkPathHash, Value = ChunkOffsets>;

    /// Maps a data root to the list of submodule-relative start offsets
    table StartOffsetsByDataRoot<Key = DataRoot, Value = RelativeStartOffsets>;

}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, Compact)]
/// chunk offsets
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct ChunkOffsets(pub Vec<ChunkOffset>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, Compact)]
/// compound value, containing the data path and tx path hashes
pub struct ChunkPathHashes {
    pub data_path_hash: Option<H256>, // ChunkPathHash - we can't use the alias types as proc_macro just deals with tokens
    pub tx_path_hash: Option<H256>,   // TxPathHash
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, Compact)]
/// chunk offsets
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct RelativeStartOffsets(pub Vec<RelativeChunkOffset>);

#[test]
fn test_offset_range_queries() -> eyre::Result<()> {
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use reth_db::cursor::*;
    use reth_db::transaction::*;

    let temp_dir = setup_tracing_and_temp_dir(Some("test_offset_range_queries"), false);

    let db = open_or_create_db(temp_dir, SubmoduleTables::ALL, None).unwrap();

    let write_tx = db.tx_mut()?;

    let data_path_hash = H256::random();
    let tx_path_hash = H256::random();

    let path_hashes = ChunkPathHashes {
        data_path_hash: Some(data_path_hash),
        tx_path_hash: Some(tx_path_hash),
    };

    write_tx.put::<ChunkPathHashByOffset>(1, path_hashes.clone())?;
    write_tx.put::<ChunkPathHashByOffset>(100, path_hashes.clone())?;
    write_tx.put::<ChunkPathHashByOffset>(0, path_hashes.clone())?;

    write_tx.commit()?;

    let read_tx = db.tx()?;

    let mut read_cursor = read_tx.cursor_read::<ChunkPathHashByOffset>()?;

    let walker = read_cursor.walk(None)?;

    let res = walker.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        res,
        vec![
            (0, path_hashes.clone()),
            (1, path_hashes.clone()),
            (100, path_hashes.clone())
        ]
    );

    Ok(())
}
