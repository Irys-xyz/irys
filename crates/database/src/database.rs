use std::path::Path;

use crate::db_cache::{
    CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata, CachedDataRoot,
};
use crate::tables::{
    CachedChunks, CachedChunksIndex, CachedDataRoots, IrysBlockHeaders, IrysBlockHeadersByHeight, IrysCommitments, 
    IrysPoAChunks, IrysTxHeaders, Metadata, PeerListItems,
};

use crate::metadata::MetadataKey;
use irys_types::{
    Address, BlockHash, ChunkPathHash, CommitmentTransaction, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionId, PeerListItem, TxChunkOffset, UnpackedChunk, MEGABYTE,
    U256,
};
use reth_db::cursor::DbDupCursorRO;

use reth_db::table::Table;
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::{
    create_db as reth_create_db,
    cursor::*,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
    ClientVersion, DatabaseEnv, DatabaseError,
};
use reth_db::{HasName, HasTableType, PlainAccountState};
use reth_node_metrics::recorder::install_prometheus_recorder;
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

pub fn open_or_create_cache_db<P: AsRef<Path>, T: HasName + HasTableType>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
) -> eyre::Result<DatabaseEnv> {
    let args = args.unwrap_or(
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((50 * MEGABYTE).try_into()?)
            .with_shrink_threshold((100 * MEGABYTE).try_into()?),
    );
    open_or_create_db(path, tables, Some(args))
}

/// Inserts a [`IrysBlockHeader`] into [`IrysBlockHeaders`]
pub fn insert_block_header<T: DbTxMut>(tx: &T, block: &IrysBlockHeader) -> eyre::Result<()> {
    tx.put::<IrysBlockHeadersByHeight>(block.height, block.block_hash)?;
    if let Some(chunk) = &block.poa.chunk {
        tx.put::<IrysPoAChunks>(block.block_hash, chunk.clone().into())?;
    };
    let mut block_without_chunk = block.clone();
    block_without_chunk.poa.chunk = None;
    tx.put::<IrysBlockHeaders>(block.block_hash, block_without_chunk.into())?;
    Ok(())
}
/// Gets a [`IrysBlockHeader`] by it's [`BlockHash`]
pub fn block_header_by_hash<T: DbTx>(
    tx: &T,
    block_hash: &BlockHash,
    include_chunk: bool,
) -> eyre::Result<Option<IrysBlockHeader>> {
    let mut block = tx
        .get::<IrysBlockHeaders>(*block_hash)?
        .map(IrysBlockHeader::from);

    if include_chunk {
        if let Some(ref mut b) = block {
            b.poa.chunk = tx.get::<IrysPoAChunks>(*block_hash)?.map(Into::into);
        }
    }

    Ok(block)
}

/// Gets a [`IrysBlockHeader`] by it's [`BlockHash`]
pub fn block_header_by_height<T: DbTx>(
    tx: &T,
    block_height: u64,
    include_chunk: bool,
) -> eyre::Result<Option<IrysBlockHeader>> {
    Ok(tx
        .get::<IrysBlockHeadersByHeight>(block_height)?
        .map(|block_hash| block_header_by_hash(tx, &block_hash, include_chunk))
        .transpose()?
        .flatten()
        .map(IrysBlockHeader::from))
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
        .map(IrysTransactionHeader::from))
}

/// Inserts a [`CommitmentTransaction`] into [`IrysCommitments`]
pub fn insert_commitment_tx<T: DbTxMut>(
    tx: &T,
    commitment_tx: &CommitmentTransaction,
) -> eyre::Result<()> {
    Ok(tx.put::<IrysCommitments>(commitment_tx.id, commitment_tx.clone().into())?)
}

/// Gets a [`CommitmentTransaction`] by it's [`IrysTransactionId`]
pub fn commitment_tx_by_txid<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
) -> eyre::Result<Option<CommitmentTransaction>> {
    Ok(tx
        .get::<IrysCommitments>(*txid)?
        .map(CommitmentTransaction::from))
}

/// Takes an [`IrysTransactionHeader`] and caches its `data_root` and tx.id in a
/// cache database table ([`CachedDataRoots`]). Tracks all the tx.ids' that share the same `data_root`.
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
        txid_set: vec![tx_header.id],
    });

    // If the entry exists, update the timestamp and add the txid if necessary
    if !cached_data_root.txid_set.contains(&tx_header.id) {
        cached_data_root.txid_set.push(tx_header.id);
    }
    cached_data_root.timestamp = timestamp;

    // Update the database with the modified or new entry
    tx.put::<CachedDataRoots>(key, cached_data_root.clone())?;

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
    chunk_offset: TxChunkOffset,
) -> eyre::Result<Option<CachedChunkIndexMetadata>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
    Ok(cursor
        .seek_by_key_subkey(data_root, *chunk_offset)?
        // make sure we find the exact subkey - dupsort seek can seek to the value, or a value greater than if it doesn't exist.
        .filter(|result| result.index == chunk_offset)
        .map(|index_entry| index_entry.meta))
}
/// Retrieves a cached chunk ([`(CachedChunkIndexMetadata, CachedChunk)`]) from the cache ([`CachedChunks`] and [`CachedChunksIndex`]) using its parent  [`DataRoot`] and [`TxRelativeChunkOffset`]
pub fn cached_chunk_by_chunk_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    chunk_offset: TxChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;

    if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, *chunk_offset)?
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
    }
}

/// Retrieves a [`CachedChunk`] from [`CachedChunks`] using its [`ChunkPathHash`]
pub fn cached_chunk_by_chunk_path_hash<T: DbTx>(
    tx: &T,
    key: &ChunkPathHash,
) -> Result<Option<CachedChunk>, DatabaseError> {
    tx.get::<CachedChunks>(*key)
}

/// Deletes [`CachedChunk`]s from [`CachedChunks`] by looking up the [`ChunkPathHash`] in [`CachedChunksIndex`]
/// It also removes the index values
pub fn delete_cached_chunks_by_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<u64> {
    let mut chunks_pruned = 0;
    // get all chunks specified by the `CachedChunksIndex`
    let mut cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut walker = cursor.walk_dup(Some(data_root), None)?; // iterate a specific key's subkeys
    while let Some((_k, c)) = walker.next().transpose()? {
        // delete them
        tx.delete::<CachedChunks>(c.meta.chunk_path_hash, None)?;
        chunks_pruned += 1;
    }
    // delete the key (and all subkeys) from the index
    tx.delete::<CachedChunksIndex>(data_root, None)?;
    Ok(chunks_pruned)
}

pub fn get_cache_size<T: Table, TX: DbTx>(tx: &TX, chunk_size: u64) -> eyre::Result<(u64, u64)> {
    let chunk_count: usize = tx.entries::<T>()?;
    Ok((chunk_count as u64, chunk_count as u64 * chunk_size))
}

/// Gets a [`IrysBlockHeader`] by it's [`BlockHash`]
pub fn get_account_balance<T: DbTx>(tx: &T, address: Address) -> eyre::Result<U256> {
    Ok(tx
        .get::<PlainAccountState>(address)?
        .map(|a| U256::from_little_endian(a.balance.as_le_slice()))
        .unwrap_or(U256::from(0)))
}

pub fn insert_peer_list_item<T: DbTxMut>(
    tx: &T,
    mining_address: &Address,
    peer_list_entry: &PeerListItem,
) -> eyre::Result<()> {
    Ok(tx.put::<PeerListItems>(mining_address.clone(), peer_list_entry.clone().into())?)
}

pub fn walk_all<T: Table, TX: DbTx>(
    read_tx: &TX,
) -> eyre::Result<Vec<(<T as Table>::Key, <T as Table>::Value)>> {
    let mut read_cursor = read_tx.cursor_read::<T>()?;
    let walker = read_cursor.walk(None)?;
    Ok(walker.collect::<Result<Vec<_>, _>>()?)
}

pub fn set_database_schema_version<T: DbTxMut>(tx: &T, version: u32) -> Result<(), DatabaseError> {
    tx.put::<Metadata>(MetadataKey::DBSchemaVersion, version.to_le_bytes().to_vec())
}

pub fn database_schema_version<T: DbTx>(tx: &T) -> Result<Option<u32>, DatabaseError> {
    if let Some(bytes) = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)? {
        let arr: [u8; 4] = bytes.as_slice().try_into().map_err(|_| {
            DatabaseError::Other(
                "Db schema version metadata does not have exactly 4 bytes".to_string(),
            )
        })?;

        Ok(Some(u32::from_le_bytes(arr)))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use irys_types::{CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader, H256};
    use reth_db::Database;

    use crate::{
        block_header_by_hash, commitment_tx_by_txid, config::get_data_dir, insert_commitment_tx,
        tables::IrysTables,
    };

    use super::{insert_block_header, insert_tx_header, open_or_create_db, tx_header_by_txid};

    #[test]
    fn insert_and_get_tests() -> eyre::Result<()> {
        let path = get_data_dir();
        println!("TempDir: {:?}", path);

        let tx_header = IrysTransactionHeader::default();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        // Write a Tx
        let _ = db.update(|tx| insert_tx_header(tx, &tx_header))?;

        // Read a Tx
        let result = db.view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?;
        assert_eq!(result, Some(tx_header));

        // Write a commitment tx
        let commitment_tx = CommitmentTransaction {
            // Override some defaults to insure deserialization is working
            id: H256::from([10u8; 32]),
            version: 1,
            ..Default::default()
        };
        let _ = db.update(|tx| insert_commitment_tx(tx, &commitment_tx))?;

        // Read a commitment tx
        let result = db.view_eyre(|tx| commitment_tx_by_txid(tx, &commitment_tx.id))?;
        assert_eq!(result, Some(commitment_tx));

        let mut block_header = IrysBlockHeader::new_mock_header();
        block_header.block_hash.0[0] = 1;

        // Write a Block
        let _ = db.update(|tx| insert_block_header(tx, &block_header))?;

        // Read a Block
        let result = db.view_eyre(|tx| block_header_by_hash(tx, &block_header.block_hash, true))?;
        let result2 = db
            .view_eyre(|tx| block_header_by_hash(tx, &block_header.block_hash, false))?
            .unwrap();

        assert_eq!(result, Some(block_header.clone()));

        // check block is retrieved without its chunk
        block_header.poa.chunk = None;
        assert_eq!(result2, block_header);
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
