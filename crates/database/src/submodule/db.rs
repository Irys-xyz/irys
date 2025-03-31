use std::path::Path;

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, RelativeChunkOffset, TxPath,
    TxPathHash, UnpackedChunk,
};
use reth_db::{
    transaction::{DbTx, DbTxMut},
    DatabaseEnv,
};

use crate::open_or_create_db;

use super::tables::{
    ChunkDataPathByPathHash, ChunkOffsetsByPathHash, ChunkPathHashByOffset, ChunkPathHashes,
    DataSizeByDataRoot, RelativeStartOffsets, StartOffsetsByDataRoot, SubmoduleTables,
    TxPathByTxPathHash,
};

/// Creates or opens a *submodule* MDBX database
pub fn create_or_open_submodule_db<P: AsRef<Path>>(path: P) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, SubmoduleTables::ALL, None)
}

/// writes a chunk's data path to the database using the provided write transaction
pub fn write_chunk_data_path<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    data_path: ChunkDataPath,
    // optional path hash, computed from data_path if None
    path_hash: Option<ChunkPathHash>,
) -> eyre::Result<()> {
    let path_hash = path_hash.unwrap_or_else(|| UnpackedChunk::hash_data_path(&data_path));
    add_offset_for_path_hash(tx, offset, path_hash)?;

    add_data_path_hash_to_offset_index(tx, offset, Some(path_hash))
}

/// writes a chunk's data path to the database using the provided transaction
pub fn add_offset_for_path_hash<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
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
    Ok(tx.get::<ChunkPathHashByOffset>(offset)?)
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
    Ok(tx.put::<ChunkPathHashByOffset>(offset, path_hashes)?)
}

/// get all the start offsets for the `data_root`
pub fn get_start_offsets_by_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<RelativeStartOffsets>> {
    Ok(tx.get::<StartOffsetsByDataRoot>(data_root)?)
}

/// set (overwrite) all the start offsets for the `data_root`
pub fn set_start_offsets_by_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    start_offsets: RelativeStartOffsets,
) -> eyre::Result<()> {
    Ok(tx.put::<StartOffsetsByDataRoot>(data_root, start_offsets)?)
}

///add a start offset to the start offsets for the `data_root`
pub fn add_start_offset_to_data_root_index<T: DbTxMut + DbTx>(
    tx: &T,
    data_root: DataRoot,
    start_offset: RelativeChunkOffset,
) -> eyre::Result<()> {
    let mut offsets = get_start_offsets_by_data_root(tx, data_root)?.unwrap_or_default();
    if !offsets.0.contains(&start_offset) {
        offsets.0.push(start_offset);
    }
    set_start_offsets_by_data_root(tx, data_root, offsets)?;
    Ok(())
}

pub fn get_data_size_by_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<u64>> {
    Ok(tx.get::<DataSizeByDataRoot>(data_root)?)
}

pub fn set_data_size_for_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    data_size: u64,
) -> eyre::Result<()> {
    Ok(tx.put::<DataSizeByDataRoot>(data_root, data_size)?)
}

/// clear db
pub fn clear_submodule_database<T: DbTxMut>(tx: &T) -> eyre::Result<()> {
    tx.clear::<ChunkPathHashByOffset>()?;
    tx.clear::<ChunkDataPathByPathHash>()?;
    tx.clear::<TxPathByTxPathHash>()?;
    tx.clear::<ChunkOffsetsByPathHash>()?;
    tx.clear::<StartOffsetsByDataRoot>()?;
    Ok(())
}
