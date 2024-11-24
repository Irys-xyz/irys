use std::path::Path;

use irys_types::{Chunk, ChunkOffset, ChunkPathHash, DataPath};
use reth_db::{
    transaction::{DbTx, DbTxMut},
    Database, DatabaseEnv,
};

use crate::open_or_create_db;

use super::tables::{ChunkOffsetsByPathHash, ChunkPathHashByOffset, SubmoduleTables};

/// Creates or opens a *submodule* MDBX database
pub fn create_or_open_submodule_db<P: AsRef<Path>>(path: P) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, SubmoduleTables::ALL, None)
}

/// writes a chunk's data path to the database using the provided transaction
pub fn write_data_path<T: DbTxMut + DbTx>(
    tx: &T,
    offset: ChunkOffset,
    data_path: DataPath,
    path_hash: Option<ChunkPathHash>,
) -> eyre::Result<()> {
    let path_hash = path_hash.unwrap_or_else(|| Chunk::hash_data_path(&data_path));
    add_offset_for_path_hash(tx, offset, path_hash)?;
    Ok(tx.put::<ChunkPathHashByOffset>(offset, path_hash)?)
}

/// writes a chunk's data path to the database using the provided transaction
pub fn add_offset_for_path_hash<T: DbTxMut + DbTx>(
    tx: &T,
    offset: ChunkOffset,
    path_hash: ChunkPathHash,
) -> eyre::Result<()> {
    let mut offsets = tx
        .get::<ChunkOffsetsByPathHash>(path_hash)?
        .unwrap_or_default();

    // this can be slow, we expect that in 99% of cases, ChunkOffsets will only have 1 element
    if !offsets.0.contains(&offset) {
        offsets.0.push(offset);
    }

    Ok(tx.put::<ChunkOffsetsByPathHash>(path_hash, offsets)?)
}

/// gets a chunk's datapath from the database using the provided transaction
pub fn read_data_path<T: DbTx>(tx: &T, offset: ChunkOffset) -> eyre::Result<Option<DataPath>> {
    Ok(tx.get::<ChunkPathHashByOffset>(offset)?)
}
