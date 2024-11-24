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
    table ChunkPathByOffset<Key = ChunkOffset, Value = DataPath>;
}

#[test]
fn test_offset_range_queries() -> eyre::Result<()> {
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use reth_db::cursor::*;
    use reth_db::transaction::*;

    let temp_dir = setup_tracing_and_temp_dir(Some("test_offset_range_queries"), false);

    let db = open_or_create_db(temp_dir, SubmoduleTables::ALL, None).unwrap();

    let write_tx = db.tx_mut()?;

    let d = vec![0, 1, 2, 3, 4];

    write_tx.put::<ChunkPathByOffset>(1, d.clone())?;
    write_tx.put::<ChunkPathByOffset>(100, d.clone())?;
    write_tx.put::<ChunkPathByOffset>(0, d.clone())?;

    write_tx.commit()?;

    let read_tx = db.tx()?;

    let mut read_cursor = read_tx.cursor_read::<ChunkPathByOffset>()?;

    let walker = read_cursor.walk(None)?;

    let res = walker.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(res, vec![(0, d.clone()), (1, d.clone()), (100, d.clone())]);

    Ok(())
}
