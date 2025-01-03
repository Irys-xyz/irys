use std::path::Path;

use crate::db_cache::{
    CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata, CachedDataRoot,
};
use crate::tables::{
    CachedChunks, CachedChunksIndex, CachedDataRoots, IrysBlockHeaders, IrysTxHeaders,
    PartitionHashes, PartitionHashesByDataRoot,
};

use crate::Ledger;
use eyre::eyre;
use irys_types::partition::PartitionHash;
use irys_types::{
    hash_sha256, BlockHash, BlockRelativeChunkOffset, ChunkPathHash, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionId, TxRelativeChunkOffset, TxPath, TxRoot,
    UnpackedChunk, H256, MEGABYTE,
};
use reth::prometheus_exporter::install_prometheus_recorder;
use reth_db::cursor::{DbDupCursorRO, DupWalker};
use reth_db::mdbx::tx::Tx;
use reth_db::mdbx::{Geometry, RO};
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::{
    create_db as reth_create_db,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
    ClientVersion, Database, DatabaseEnv, DatabaseError,
};
use reth_db::{HasName, HasTableType};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Opens up an existing database or creates a new one at the specified path. Creates tables if
/// necessary. Read/Write mode.
pub fn open_or_create_db<P: AsRef<Path>, T: HasName + HasTableType>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
) -> eyre::Result<DatabaseEnv> {
    let args = args.unwrap_or(
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((10 * MEGABYTE).try_into()?),
    );

    // Register the prometheus recorder before creating the database,
    // because irys_database init needs it to register metrics.
    let _ = install_prometheus_recorder();
    let db = reth_create_db(path, args)?.with_metrics_and_tables(tables);

    Ok(db)
}
/// Inserts a [`IrysBlockHeader`] into [`IrysBlockHeaders`]
pub fn insert_block_header<T: DbTxMut>(tx: &T, block: &IrysBlockHeader) -> eyre::Result<()> {
    Ok(tx.put::<IrysBlockHeaders>(block.block_hash, block.clone().into())?)
}
/// Gets a [`IrysBlockHeader`] by it's [`BlockHash`]
pub fn block_header_by_hash<T: DbTx>(
    tx: &T,
    block_hash: &BlockHash,
) -> eyre::Result<Option<IrysBlockHeader>> {
    Ok(tx
        .get::<IrysBlockHeaders>(*block_hash)?
        .and_then(|r| Some(IrysBlockHeader::from(r))))
}

/// Inserts a [`IrysTransactionHeader`] into [`IrysTxHeaders`]
pub fn insert_tx_header<T: DbTxMut>(tx: &T, tx_header: &IrysTransactionHeader) -> eyre::Result<()> {
    Ok(tx.put::<IrysTxHeaders>(tx_header.id, tx_header.clone().into())?)
}

/// Gets a [`IrysTransactionHeader`] by it's [`IrysTransactionId`]
pub fn tx_header_by_txid<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
) -> eyre::Result<Option<IrysTransactionHeader>> {
    Ok(tx
        .get::<IrysTxHeaders>(*txid)?
        .and_then(|r| Some(IrysTransactionHeader::from(r))))
}

/// Takes an [`IrysTransactionHeader`] and caches its data_root and tx.id in a
/// cache database table ([`CachedDataRoots`]). Tracks all the tx.ids' that share the same data_root.
pub fn cache_data_root<T: DbTx + DbTxMut>(
    tx: &T,
    tx_header: &IrysTransactionHeader,
) -> eyre::Result<Option<CachedDataRoot>> {
    let key = tx_header.data_root;

    // Calculate the duration since UNIX_EPOCH
    let now = SystemTime::now();
    let duration_since_epoch = now
        .duration_since(UNIX_EPOCH)
        .expect("should be able to compute duration since UNIX_EPOCH");
    let timestamp = duration_since_epoch.as_millis();

    // Access the current cached entry from the database
    let result = tx.get::<CachedDataRoots>(key)?;

    // Create or update the CachedDataRoot
    let mut cached_data_root = result.unwrap_or_else(|| CachedDataRoot {
        timestamp,
        data_size: tx_header.data_size,
        txid_set: vec![tx_header.id.clone()],
    });

    // If the entry exists, update the timestamp and add the txid if necessary
    if !cached_data_root.txid_set.contains(&tx_header.id) {
        cached_data_root.txid_set.push(tx_header.id.clone());
    }
    cached_data_root.timestamp = timestamp;

    // Update the database with the modified or new entry
    tx.put::<CachedDataRoots>(key, cached_data_root.clone().into())?;

    Ok(Some(cached_data_root))
}

/// Gets a [`CachedDataRoot`] by it's [`DataRoot`] from [`CachedDataRoots`] .
pub fn cached_data_root_by_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<CachedDataRoot>> {
    Ok(tx.get::<CachedDataRoots>(data_root)?)
}

type IsDuplicate = bool;

/// Caches a [`Chunk`] - returns `true` if the chunk was a duplicate (present in [`CachedChunks`])
/// and was not inserted into [`CachedChunksIndex`] or [`CachedChunks`]
pub fn cache_chunk<T: DbTx + DbTxMut>(tx: &T, chunk: &UnpackedChunk) -> eyre::Result<IsDuplicate> {
    let chunk_path_hash: ChunkPathHash = chunk.chunk_path_hash();
    if cached_chunk_by_chunk_path_hash(tx, &chunk_path_hash)?.is_some() {
        warn!(
            "Chunk {} of {} is already cached, skipping..",
            &chunk_path_hash, &chunk.data_root
        );
        return Ok(true);
    }
    let value = CachedChunkIndexEntry {
        index: chunk.tx_offset,
        meta: CachedChunkIndexMetadata { chunk_path_hash },
    };

    debug!(
        "Caching chunk {} ({}) of {}",
        &chunk.tx_offset, &chunk_path_hash, &chunk.data_root
    );

    tx.put::<CachedChunksIndex>(chunk.data_root, value)?;
    tx.put::<CachedChunks>(chunk_path_hash, chunk.into())?;
    Ok(false)
}

/// Retrieves a cached chunk ([`CachedChunkIndexMetadata`]) from the [`CachedChunksIndex`] using its parent [`DataRoot`] and [`TxRelativeChunkOffset`]
pub fn cached_chunk_meta_by_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    chunk_offset: TxRelativeChunkOffset,
) -> eyre::Result<Option<CachedChunkIndexMetadata>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
    Ok(cursor
        .seek_by_key_subkey(data_root, chunk_offset)?
        // make sure we find the exact subkey - dupsort seek can seek to the value, or a value greater than if it doesn't exist.
        .filter(|result| result.index == chunk_offset)
        .and_then(|index_entry| Some(index_entry.meta)))
}
/// Retrieves a cached chunk ([`(CachedChunkIndexMetadata, CachedChunk)`]) from the cache ([`CachedChunks`] and [`CachedChunksIndex`]) using its parent  [`DataRoot`] and [`TxRelativeChunkOffset`]
pub fn cached_chunk_by_chunk_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    chunk_offset: TxRelativeChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;

    let result = if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, chunk_offset)?
        .filter(|e| e.index == chunk_offset)
    {
        let meta: CachedChunkIndexMetadata = index_entry.into();
        // expect that the cached chunk always has an entry if the index entry exists
        Ok(Some((
            meta.clone(),
            tx.get::<CachedChunks>(meta.chunk_path_hash)?
                .expect("Chunk has an index entry but no data entry"),
        )))
    } else {
        Ok(None)
    };

    return result;
}

/// Retrieves a [`CachedChunk`] from [`CachedChunks`] using its [`ChunkPathHash`]
pub fn cached_chunk_by_chunk_path_hash<T: DbTx>(
    tx: &T,
    key: &ChunkPathHash,
) -> Result<Option<CachedChunk>, DatabaseError> {
    Ok(tx.get::<CachedChunks>(*key)?)
}

/// Associates a partition hash with a data root, appending to existing
/// partition hashes if present or creating a new list if not. Indicates
/// that chunks of this data overlap with the partition.
pub fn assign_data_root<T: DbTxMut + DbTx>(
    tx: &T,
    data_root: DataRoot,
    partition_hash: PartitionHash,
) -> eyre::Result<()> {
    let partition_hashes = if let Some(mut phs) = get_partition_hashes_by_data_root(tx, data_root)?
    {
        if !phs.0.contains(&partition_hash) {
            phs.0.push(partition_hash)
        };
        phs
    } else {
        PartitionHashes(vec![partition_hash])
    };
    set_partition_hashes_by_data_root(tx, data_root, partition_hashes)?;
    Ok(())
}

/// Stores list of partition hashes for a data root in the database
pub fn set_partition_hashes_by_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    partition_hashes: PartitionHashes,
) -> eyre::Result<()> {
    Ok(tx.put::<PartitionHashesByDataRoot>(data_root, partition_hashes)?)
}

/// Retrieves list of partition hashes for a data root from the database
pub fn get_partition_hashes_by_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<PartitionHashes>> {
    let r = tx.get::<PartitionHashesByDataRoot>(data_root)?;
    dbg!(&r);
    Ok(r)
}

#[cfg(test)]
mod tests {
    use irys_types::{IrysBlockHeader, IrysTransactionHeader};
    use reth_db::Database;

    use crate::{block_header_by_hash, config::get_data_dir, tables::IrysTables};

    use super::{insert_block_header, insert_tx_header, open_or_create_db, tx_header_by_txid};

    #[test]
    fn insert_and_get_tests() -> eyre::Result<()> {
        //let path = tempdir().unwrap();
        let path = get_data_dir();
        println!("TempDir: {:?}", path);

        let tx_header = IrysTransactionHeader::default();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        // Write a Tx
        let _ = db.update(|tx| insert_tx_header(tx, &tx_header))?;

        // Read a Tx
        let result = db.view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?;
        assert_eq!(result, Some(tx_header));

        let mut block_header = IrysBlockHeader::new();
        block_header.block_hash.0[0] = 1;

        // Write a Block
        let _ = db.update(|tx| insert_block_header(tx, &block_header))?;

        // Read a Block
        let result = db.view_eyre(|tx| block_header_by_hash(tx, &block_header.block_hash))?;
        assert_eq!(result, Some(block_header));

        Ok(())
    }

    // #[test]
    // fn insert_and_get_a_block() {
    //     //let path = tempdir().unwrap();
    //     let path = get_data_dir();
    //     println!("TempDir: {:?}", path);

    //     let mut block_header = IrysBlockHeader::new();
    //     block_header.block_hash.0[0] = 1;
    //     let db = open_or_create_db(path).unwrap();

    //     // Write a Block
    //     {
    //         let result = insert_block(&db, &block_header);
    //         println!("result: {:?}", result);
    //         assert_matches!(result, Ok(_));
    //     }

    //     // Read a Block
    //     {
    //         let result = block_by_hash(&db, block_header.block_hash);
    //         assert_eq!(result, Ok(Some(block_header)));
    //         println!("result: {:?}", result.unwrap().unwrap());
    //     }
    // }

    // #[test]
    // fn insert_and_get_tx() {
    //     //let path = tempdir().unwrap();
    //     let path = get_data_dir();
    //     println!("TempDir: {:?}", path);

    //     let mut tx = IrysTransactionHeader::default();
    //     tx.id.0[0] = 2;
    //     let db = open_or_create_db(path).unwrap();

    //     // Write a Tx
    //     {
    //         let result = insert_tx(&db, &tx);
    //         println!("result: {:?}", result);
    //         assert_matches!(result, Ok(_));
    //     }

    //     // Read a Tx
    //     {
    //         let result = tx_by_txid(&db, &tx.id);
    //         assert_eq!(result, Ok(Some(tx)));
    //         println!("result: {:?}", result.unwrap().unwrap());
    //     }
    // }
}
