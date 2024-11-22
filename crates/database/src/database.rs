use std::path::Path;

use crate::db_cache::{
    chunk_offset_to_index, CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata,
    CachedDataRoot,
};
use crate::tables::{
    BlockRelativeTxPathIndex, CachedChunks, CachedChunksIndex, CachedDataRoots, IrysBlockHeaders,
    IrysTxHeaders, Tables,
};
use crate::tx_path::{
    BlockRelativeTxPathIndexEntry, BlockRelativeTxPathIndexKey, BlockRelativeTxPathIndexMeta,
};
use crate::Ledger;
use irys_types::{
    hash_sha256, BlockHash, BlockRelativeChunkOffset, Chunk, ChunkPathHash, DataRoot,
    IrysBlockHeader, IrysTransactionHeader, TxPath, TxRelativeChunkIndex, TxRelativeChunkOffset,
    TxRoot, H256, MEGABYTE,
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

const ERROR_GET: &str = "Not able to get value from table.";
const ERROR_PUT: &str = "Not able to insert value into table.";

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

pub fn insert_block(db: &DatabaseEnv, block: &IrysBlockHeader) -> Result<(), DatabaseError> {
    let value = block;
    let key = value.block_hash;

    db.update(|tx| {
        tx.put::<IrysBlockHeaders>(key, value.clone().into())
            .expect(ERROR_PUT)
    })
}

pub fn block_by_hash(
    db: &DatabaseEnv,
    block_hash: H256,
) -> Result<Option<IrysBlockHeader>, DatabaseError> {
    let key = block_hash;

    let result = db.view(|tx| tx.get::<IrysBlockHeaders>(key).expect(ERROR_GET))?;
    Ok(Some(IrysBlockHeader::from(result.unwrap())))
}

pub fn insert_tx(db: &DatabaseEnv, tx: &IrysTransactionHeader) -> Result<(), DatabaseError> {
    let key = tx.id;
    let value = tx;

    db.update(|tx| {
        tx.put::<IrysTxHeaders>(key, value.clone().into())
            .expect(ERROR_PUT)
    })
}

pub fn tx_by_txid(
    db: &DatabaseEnv,
    txid: &H256,
) -> Result<Option<IrysTransactionHeader>, DatabaseError> {
    let key = txid;
    let result = db.view(|tx| tx.get::<IrysTxHeaders>(*key).expect(ERROR_GET))?;
    Ok(Some(IrysTransactionHeader::from(result.unwrap())))
}

/// Takes an IrysTransactionHeader and caches its data_root and tx.id in a
/// cache database table. Tracks all the tx.ids' that share the same data_root.
pub fn cache_data_root(
    db: &DatabaseEnv,
    tx: &IrysTransactionHeader,
) -> Result<Option<CachedDataRoot>, DatabaseError> {
    let key = tx.data_root;

    // Calculate the duration since UNIX_EPOCH
    let now = SystemTime::now();
    let duration_since_epoch = now
        .duration_since(UNIX_EPOCH)
        .expect("should be able to compute duration since UNIX_EPOCH");
    let timestamp = duration_since_epoch.as_millis();

    // Access the current cached entry from the database
    let result = db.view(|tx| tx.get::<CachedDataRoots>(key).expect(ERROR_GET))?;

    // Create or update the CachedDataRoot
    let mut cached_data_root = result.unwrap_or_else(|| CachedDataRoot {
        timestamp,
        data_size: tx.data_size,
        txid_set: vec![tx.id.clone()],
    });

    // If the entry exists, update the timestamp and add the txid if necessary
    if !cached_data_root.txid_set.contains(&tx.id) {
        cached_data_root.txid_set.push(tx.id.clone());
    }
    cached_data_root.timestamp = timestamp;

    // Update the database with the modified or new entry
    db.update(|tx| {
        tx.put::<CachedDataRoots>(key, cached_data_root.clone().into())
            .expect(ERROR_PUT)
    })?;

    Ok(Some(cached_data_root))
}

/// Retrieves a CashedDataRoot struct using a data_root as key.
pub fn cached_data_root_by_data_root(
    db: &DatabaseEnv,
    data_root: DataRoot,
) -> Result<Option<CachedDataRoot>, DatabaseError> {
    let key = data_root;
    let result = db.view(|tx| tx.get::<CachedDataRoots>(key).expect(ERROR_GET))?;
    Ok(result)
}

type IsDuplicate = bool;
/// Caches a chunk - returns `true` if the chunk was a duplicate and was not inserted
pub fn cache_chunk(db: &DatabaseEnv, chunk: Chunk) -> eyre::Result<IsDuplicate> {
    let chunk_index = chunk_offset_to_index(chunk.offset)?;
    let chunk_path_hash: ChunkPathHash = hash_sha256(&chunk.data_path.0).unwrap().into();
    if cached_chunk_by_chunk_key(db, chunk_path_hash)?.is_some() {
        warn!(
            "Chunk {} of {} is already cached, skipping..",
            &chunk_path_hash, &chunk.data_root
        );
        return Ok(true);
    }
    let value = CachedChunkIndexEntry {
        index: chunk_index,
        meta: CachedChunkIndexMetadata { chunk_path_hash },
    };

    debug!(
        "Caching chunk {} ({}) of {}",
        &chunk_index, &chunk_path_hash, &chunk.data_root
    );
    db.update(|tx: &Tx<reth_db::mdbx::RW>| {
        tx.put::<CachedChunksIndex>(chunk.data_root, value)
            .expect(ERROR_PUT);
        tx.put::<CachedChunks>(chunk_path_hash, chunk.into())
            .expect(ERROR_PUT);
    })?;
    Ok(false)
}

/// Retrieves a cached chunk from the cache using its parent data root and offset
pub fn cached_chunk_meta_by_offset(
    db: &DatabaseEnv,
    data_root: DataRoot,
    chunk_offset: TxRelativeChunkOffset,
) -> eyre::Result<Option<CachedChunkIndexMetadata>> {
    let chunk_index = chunk_offset_to_index(chunk_offset)?;
    let tx = db.tx()?;
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
    let result = if let Some(result) = cursor.seek_by_key_subkey(data_root, chunk_index)? {
        if result.index == chunk_index {
            Ok(Some(result.meta))
        } else {
            Ok(None)
        }
    } else {
        Ok(None)
    };
    tx.commit()?;
    return result;
}
/// Retrieves a cached chunk from the cache using its parent data root and offset
pub fn cached_chunk_by_offset(
    db: &DatabaseEnv,
    data_root: DataRoot,
    chunk_offset: TxRelativeChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    let chunk_index = chunk_offset_to_index(chunk_offset)?;
    let tx = db.tx()?;

    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;

    let result = if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, chunk_index)?
        .filter(|e| e.index == chunk_index)
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
    tx.commit()?;

    return result;
}

/// Retrieves a cached chunk from the cache using its chunk_key (the sha256
/// hash of the data_path bytes)
pub fn cached_chunk_by_chunk_key(
    db: &DatabaseEnv,
    key: H256,
) -> Result<Option<CachedChunk>, DatabaseError> {
    let result = db.view(|tx| tx.get::<CachedChunks>(key).expect(ERROR_GET))?;
    Ok(result)
}

/// get the associated tx path for a block & ledger relative offset
/// NOTE: this function ASSUMES that the index is VALID, specifically that it has no missing entries - as it "rounds up" the offset to the nearest entry to fufill the range component
pub fn get_tx_path_by_block_ledger_offset(
    db: &DatabaseEnv,
    block_hash: BlockHash,
    ledger: Ledger,
    // offset is inclusive
    chunk_offset: BlockRelativeChunkOffset,
) -> Result<Option<TxPath>, DatabaseError> {
    let tx = db.tx()?;
    let mut cursor = tx.cursor_dup_read::<BlockRelativeTxPathIndex>()?;

    Ok(cursor
        .seek_by_key_subkey(
            BlockRelativeTxPathIndexKey { block_hash, ledger },
            chunk_offset,
        )?
        .map(|e| e.meta.tx_path))
}

/// Stores the provided tx path under a compound key of block_hash + ledger, with a ranged subkey of end_offset
pub fn store_tx_path_by_block_offset(
    db: &DatabaseEnv,
    block_hash: BlockHash,
    ledger: Ledger,
    // this should be the offset of the *last* chunk for this tx path, inclusive.
    end_chunk_offset: BlockRelativeChunkOffset,
    tx_path: TxPath,
) -> Result<(), DatabaseError> {
    let key = BlockRelativeTxPathIndexKey { block_hash, ledger };
    let subkey = BlockRelativeTxPathIndexEntry {
        end_offset: end_chunk_offset,
        meta: BlockRelativeTxPathIndexMeta { tx_path },
    };
    let write_tx = db.tx_mut()?;
    write_tx.put::<BlockRelativeTxPathIndex>(key, subkey)?;
    write_tx.commit()?;

    Ok(())
}

#[cfg(test)]
mod tests {

    use assert_matches::assert_matches;
    use irys_types::{IrysBlockHeader, IrysTransactionHeader};
    //use tempfile::tempdir;

    use crate::{block_by_hash, config::get_data_dir, tables::Tables};

    use super::{insert_block, open_or_create_db};

    #[test]
    fn insert_and_get_tests() {
        //let path = tempdir().unwrap();
        let path = get_data_dir();
        println!("TempDir: {:?}", path);

        let mut tx = IrysTransactionHeader::default();
        tx.id.0[0] = 2;
        let db = open_or_create_db(path, Tables::ALL).unwrap();

        // // Write a Tx
        // {
        //     let result = insert_tx(&db, &tx);
        //     println!("result: {:?}", result);
        //     assert_matches!(result, Ok(_));
        // }

        // // Read a Tx
        // {
        //     let result = tx_by_txid(&db, &tx.id);
        //     assert_eq!(result, Ok(Some(tx)));
        //     println!("result: {:?}", result.unwrap().unwrap());
        // }

        let mut block_header = IrysBlockHeader::new();
        block_header.block_hash.0[0] = 1;

        // Write a Block
        {
            let result = insert_block(&db, &block_header);
            println!("result: {:?}", result);
            assert_matches!(result, Ok(_));
        }

        // Read a Block
        {
            let result = block_by_hash(&db, block_header.block_hash);
            assert_eq!(result, Ok(Some(block_header)));
            println!("result: {:?}", result.unwrap().unwrap());
        }
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
