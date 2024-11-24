use irys_types::{
    ingress::IngressProof, ChunkOffset, ChunkPathHash, DataPath, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, TxRelativeChunkIndex, H256,
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
    /// Index that maps a partition-relative offset to a DataPath
    /// note: mdbx keys are always sorted, so range queries work :)
    /// TODO: use custom Compact impl for Vec<u8> so we don't have problems

    /// Maps a partition relative offset to a chunk path hash
    table ChunkPathHashByOffset<Key = ChunkOffset, Value = ChunkPathHash>;

    /// Maps a chunk path hash to the list of submodule-relative offsets it should inhabit
    table ChunkOffsetsByPathHash<Key = ChunkPathHash, Value = ChunkOffsets>;

}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, Compact)]
/// chunk offsets
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct ChunkOffsets(pub Vec<ChunkOffset>);

#[test]
fn test_offset_range_queries() -> eyre::Result<()> {
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use reth_db::cursor::*;
    use reth_db::transaction::*;

    let temp_dir = setup_tracing_and_temp_dir(Some("test_offset_range_queries"), false);

    let db = open_or_create_db(temp_dir, SubmoduleTables::ALL, None).unwrap();

    let write_tx = db.tx_mut()?;

    let d = vec![0, 1, 2, 3, 4];
    let chunk_path_hash = H256::random();
    write_tx.put::<ChunkPathHashByOffset>(1, chunk_path_hash)?;
    write_tx.put::<ChunkPathHashByOffset>(100, chunk_path_hash)?;
    write_tx.put::<ChunkPathHashByOffset>(0, chunk_path_hash)?;

    write_tx.commit()?;

    let read_tx = db.tx()?;

    let mut read_cursor = read_tx.cursor_read::<ChunkPathHashByOffset>()?;

    let walker = read_cursor.walk(None)?;

    let res = walker.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        res,
        vec![
            (0, chunk_path_hash),
            (1, chunk_path_hash),
            (100, chunk_path_hash)
        ]
    );

    Ok(())
}
