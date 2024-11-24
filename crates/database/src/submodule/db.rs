use std::path::Path;

use irys_types::{ChunkOffset, DataPath};
use reth_db::{
    transaction::{DbTx, DbTxMut},
    Database, DatabaseEnv,
};

use crate::open_or_create_db;

use super::tables::{ChunkPathByOffset, SubmoduleTables};

/// Creates or opens a *submodule* MDBX database
pub fn create_or_open_submodule_db<P: AsRef<Path>>(path: P) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, SubmoduleTables::ALL, None)
}

/// writes a chunk's data path to the database using the provided transaction
pub fn write_data_path<T: DbTxMut>(
    tx: &T,
    offset: ChunkOffset,
    data_path: DataPath,
) -> eyre::Result<()> {
    Ok(tx.put::<ChunkPathByOffset>(offset, data_path)?)
}

/// gets a chunk's datapath from the database using the provided transaction
pub fn read_data_path<T: DbTx>(tx: &T, offset: ChunkOffset) -> eyre::Result<Option<DataPath>> {
    Ok(tx.get::<ChunkPathByOffset>(offset)?)
}
