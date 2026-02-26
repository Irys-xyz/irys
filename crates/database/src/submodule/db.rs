use std::path::Path;

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, MEGABYTE, PartitionChunkOffset, TERABYTE, TxPath,
    TxPathHash,
};
use reth_db::{
    ClientVersion, DatabaseEnv,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
    transaction::{DbTx, DbTxMut},
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
    open_or_create_db(
        path,
        SubmoduleTables::ALL,
        Some(
            DatabaseArguments::new(ClientVersion::default())
                .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
                // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
                .with_growth_step((10 * MEGABYTE).into())
                .with_shrink_threshold((20 * MEGABYTE).try_into()?)
                .with_geometry_max_size(Some(2 * TERABYTE)),
        ),
    )
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
    info: &DataRootInfo,
) -> eyre::Result<()> {
    let mut data_root_infos = get_data_root_infos_for_data_root(tx, data_root)?.unwrap_or_default();
    if !data_root_infos.0.contains(info) {
        data_root_infos.0.push(info.clone());
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

#[cfg(test)]
mod tests {
    use crate::db::IrysDatabaseExt as _;
    use crate::submodule::tables::DataRootInfos;
    use crate::submodule::{
        add_data_root_info, get_data_root_infos_for_data_root, set_data_root_infos_for_data_root,
    };
    use crate::{
        open_or_create_db,
        submodule::tables::{DataRootInfo, SubmoduleTables},
    };
    use irys_types::H256;
    use irys_types::RelativeChunkOffset;
    use reth_db::Database as _;
    use reth_db::transaction::DbTx as _;

    #[test]
    fn db_compact_dataroot_info() -> eyre::Result<()> {
        let builder = tempfile::Builder::new()
            .prefix("irys-datarootinfo-")
            .rand_bytes(8)
            .tempdir();
        let tmpdir = builder
            .expect("Not able to create a temporary directory.")
            .keep();

        let db = open_or_create_db(tmpdir, SubmoduleTables::ALL, None)?;
        let write_tx = db.tx_mut()?;
        let infos = vec![
            DataRootInfo {
                start_offset: RelativeChunkOffset(0),
                data_size: 0,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(-20),
                data_size: 100,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(10000),
                data_size: 4000,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(i32::MIN),
                data_size: u64::MAX,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(i32::MAX),
                data_size: u64::MAX,
            },
        ];
        let data_root = H256::zero();
        set_data_root_infos_for_data_root(&write_tx, data_root, DataRootInfos(infos.clone()))?;
        write_tx.commit()?;

        let infos2 = db
            .view_eyre(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            .unwrap();

        assert_eq!(infos, infos2.0);

        // add a DataRootInfo to existing DataRootInfos
        let new_data_root_info = DataRootInfo {
            start_offset: RelativeChunkOffset(1),
            data_size: 100,
        };

        db.update_eyre(|tx| add_data_root_info(tx, data_root, &new_data_root_info))?;

        let infos3 = db
            .view_eyre(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            .unwrap();

        assert_eq!(infos3.0.len(), 6);
        assert_eq!(*infos3.0.last().unwrap(), new_data_root_info);

        let random_data_root = H256::random();
        let missing_infos =
            db.view_eyre(|tx| get_data_root_infos_for_data_root(tx, random_data_root))?;

        assert!(missing_infos.is_none());

        Ok(())
    }
}
