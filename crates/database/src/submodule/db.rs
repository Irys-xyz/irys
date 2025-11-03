use std::path::Path;

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::{
    transaction::{DbTx, DbTxMut},
    DatabaseEnv,
};

use crate::{
    open_or_create_db,
    submodule::tables::{DataRootInfo, DataRootInfosByDataRoot},
};

use super::tables::{
    ChunkDataPathByPathHash, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfos,
    SubmoduleTables, TxPathByTxPathHash,
};

/// Creates or opens a *submodule* MDBX database
pub fn create_or_open_submodule_db<P: AsRef<Path>>(path: P) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, SubmoduleTables::ALL, None)
}

/// gets the full data path for the chunk with the provided offset
pub fn get_data_path_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<ChunkDataPath>> {
    if let Some(data_path_hash) =
        get_path_hashes_by_offset(tx, offset)?.and_then(|h| h.data_path_hash)
    {
        Ok(get_full_data_path(tx, data_path_hash)?)
    } else {
        Ok(None)
    }
}

/// gets the full tx path for the chunk with the provided offset
pub fn get_tx_path_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<TxPath>> {
    if let Some(tx_path_hash) = get_path_hashes_by_offset(tx, offset)?.and_then(|h| h.tx_path_hash)
    {
        Ok(get_full_tx_path(tx, tx_path_hash)?)
    } else {
        Ok(None)
    }
}

pub fn get_path_hashes_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<ChunkPathHashes>> {
    Ok(tx.get::<ChunkPathHashesByOffset>(offset)?)
}

pub fn get_full_data_path<T: DbTx>(
    tx: &T,
    path_hash: ChunkPathHash,
) -> eyre::Result<Option<ChunkDataPath>> {
    Ok(tx.get::<ChunkDataPathByPathHash>(path_hash)?)
}

pub fn add_full_data_path<T: DbTxMut>(
    tx: &T,
    path_hash: ChunkPathHash,
    data_path: ChunkDataPath,
) -> eyre::Result<()> {
    tx.put::<ChunkDataPathByPathHash>(path_hash, data_path)?;
    Ok(())
}

pub fn get_full_tx_path<T: DbTx>(tx: &T, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
    Ok(tx.get::<TxPathByTxPathHash>(path_hash)?)
}

pub fn add_full_tx_path<T: DbTxMut>(
    tx: &T,
    path_hash: TxPathHash,
    tx_path: TxPath,
) -> eyre::Result<()> {
    tx.put::<TxPathByTxPathHash>(path_hash, tx_path)?;
    Ok(())
}

pub fn add_data_path_hash_to_offset_index<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hash: Option<ChunkPathHash>,
) -> eyre::Result<()> {
    let mut chunk_hashes = get_path_hashes_by_offset(tx, offset)?.unwrap_or_default();
    chunk_hashes.data_path_hash = path_hash;
    set_path_hashes_by_offset(tx, offset, chunk_hashes)?;
    Ok(())
}

pub fn add_tx_path_hash_to_offset_index<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hash: Option<TxPathHash>,
) -> eyre::Result<()> {
    let mut chunk_hashes = get_path_hashes_by_offset(tx, offset)?.unwrap_or_default();
    chunk_hashes.tx_path_hash = path_hash;
    set_path_hashes_by_offset(tx, offset, chunk_hashes)?;
    Ok(())
}

pub fn set_path_hashes_by_offset<T: DbTxMut>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hashes: ChunkPathHashes,
) -> eyre::Result<()> {
    Ok(tx.put::<ChunkPathHashesByOffset>(offset, path_hashes)?)
}

/// get DataRootInfos for the `data_root`
pub fn get_data_root_infos_for_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<DataRootInfos>> {
    Ok(tx.get::<DataRootInfosByDataRoot>(data_root)?)
}

/// set (overwrite) the DataRootInfos for the `data_root`
pub fn set_data_root_infos_for_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    infos: DataRootInfos,
) -> eyre::Result<()> {
    Ok(tx.put::<DataRootInfosByDataRoot>(data_root, infos)?)
}

/// Track the start_offset and data_size metadata for the `data_root`
pub fn add_data_root_info<T: DbTxMut + DbTx>(
    tx: &T,
    data_root: DataRoot,
    info: DataRootInfo,
) -> eyre::Result<()> {
    let mut data_root_infos = get_data_root_infos_for_data_root(tx, data_root)?.unwrap_or_default();
    if !data_root_infos.0.contains(&info) {
        data_root_infos.0.push(info);
    }
    set_data_root_infos_for_data_root(tx, data_root, data_root_infos)?;
    Ok(())
}

/// clear db
pub fn clear_submodule_database<T: DbTxMut>(tx: &T) -> eyre::Result<()> {
    tx.clear::<ChunkPathHashesByOffset>()?;
    tx.clear::<ChunkDataPathByPathHash>()?;
    tx.clear::<TxPathByTxPathHash>()?;
    tx.clear::<DataRootInfosByDataRoot>()?;
    Ok(())
}
