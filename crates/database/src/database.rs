use std::path::Path;

use crate::db_cache::{
    CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata, CachedDataRoot,
};
use crate::tables::{
    CachedChunks, CachedChunksIndex, CachedDataRoots, CompactCachedIngressProof, IngressProofs,
    IrysBlockHeaders, IrysCommitments, IrysDataTxHeaders, IrysPoAChunks, Metadata, PeerListItems,
};

use crate::metadata::MetadataKey;
use crate::reth_ext::IrysRethDatabaseEnvMetricsExt as _;
use irys_types::ingress::CachedIngressProof;
use irys_types::irys::IrysSigner;
use irys_types::{
    BlockHash, ChunkPathHash, CommitmentTransaction, DataRoot, DataTransactionHeader,
    DatabaseProvider, IngressProof, IrysAddress, IrysBlockHeader, IrysPeerId, IrysTransactionId,
    PeerListItem, TxChunkOffset, UnixTimestamp, UnpackedChunk, H256, MEGABYTE,
};
use reth_db::cursor::DbDupCursorRO as _;
use reth_db::mdbx::init_db_for;
use reth_db::table::{Table, TableInfo};
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::TableSet;
use reth_db::{
    cursor::*,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
    ClientVersion, DatabaseEnv, DatabaseError,
};
use reth_db_api::Database as _;
use reth_node_metrics::recorder::install_prometheus_recorder;
use tracing::{debug, warn};

/// Opens up an existing database or creates a new one at the specified path. Creates tables if
/// necessary. Read/Write mode.
pub fn open_or_create_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
) -> eyre::Result<DatabaseEnv> {
    let args = args.unwrap_or(
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((10 * MEGABYTE).into())
            .with_shrink_threshold((20 * MEGABYTE).try_into()?),
    );

    // Register the prometheus recorder before creating the database,
    // because irys_database init needs it to register metrics.
    let _ = install_prometheus_recorder();
    let db = init_db_for::<P, T>(path, args)?.with_metrics_and_tables(tables);

    Ok(db)
}

pub fn open_or_create_cache_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
) -> eyre::Result<DatabaseEnv> {
    let args = args.unwrap_or(
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((50 * MEGABYTE).into())
            .with_shrink_threshold((100 * MEGABYTE).try_into()?),
    );
    open_or_create_db(path, tables, Some(args))
}

/// Inserts a [`IrysBlockHeader`] into [`IrysBlockHeaders`]
pub fn insert_block_header<T: DbTxMut>(tx: &T, block: &IrysBlockHeader) -> eyre::Result<()> {
    if let Some(chunk) = &block.poa.chunk {
        tx.put::<IrysPoAChunks>(block.block_hash, chunk.clone().into())?;
    } else {
        tracing::error!(block.hash = ?block.block_hash, target = "db::block_header", "poa chunk not present when writing the header");
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
    // TODO: we should be typing these Results correctly (DatabaseError instead of eyre::Report)
) -> eyre::Result<Option<IrysBlockHeader>> {
    let mut block = tx
        .get::<IrysBlockHeaders>(*block_hash)?
        .map(IrysBlockHeader::from);

    if include_chunk {
        if let Some(ref mut b) = block {
            b.poa.chunk = tx.get::<IrysPoAChunks>(*block_hash)?.map(Into::into);
            if b.poa.chunk.is_none() && b.height != 0 {
                tracing::error!(block.hash = ?b.block_hash, height = b.height,  target = "db::block_header", "poa chunk not present when reading the header");
            }
        }
    }

    Ok(block)
}

/// Inserts a [`DataTransactionHeader`] into [`IrysDataTxHeaders`]
pub fn insert_tx_header<T: DbTxMut>(tx: &T, tx_header: &DataTransactionHeader) -> eyre::Result<()> {
    Ok(tx.put::<IrysDataTxHeaders>(tx_header.id, tx_header.clone().into())?)
}

/// Gets a [`DataTransactionHeader`] by it's [`IrysTransactionId`]
pub fn tx_header_by_txid<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
) -> eyre::Result<Option<DataTransactionHeader>> {
    if let Some(mut header) = tx
        .get::<IrysDataTxHeaders>(*txid)?
        .map(DataTransactionHeader::from)
    {
        // Load metadata from separate table if it exists
        if let Some(metadata) = crate::get_data_tx_metadata(tx, txid)? {
            *header.metadata_mut() = metadata;
        }
        Ok(Some(header))
    } else {
        Ok(None)
    }
}

/// Inserts a [`CommitmentTransaction`] into [`IrysCommitments`]
pub fn insert_commitment_tx<T: DbTxMut>(
    tx: &T,
    commitment_tx: &CommitmentTransaction,
) -> eyre::Result<()> {
    Ok(tx.put::<IrysCommitments>(commitment_tx.id(), commitment_tx.clone().into())?)
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

/// Confirms the data size of a cached data root entry after validating its rightmost chunk.
///
/// Updates the entry with the proven data size extracted from the last leaf's merkle proof.
/// Returns `Ok(Some(entry))` if updated, `Ok(None)` if entry not found.
pub fn confirm_data_size_for_data_root<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: &H256,
    data_size: u64,
) -> eyre::Result<Option<CachedDataRoot>> {
    // Access the current cached entry from the database
    let result = tx.get::<CachedDataRoots>(*data_root)?;

    if let Some(mut cached_data_root) = result {
        cached_data_root.data_size = data_size;
        cached_data_root.data_size_confirmed = true;
        tx.put::<CachedDataRoots>(*data_root, cached_data_root.clone())?;
        Ok(Some(cached_data_root))
    } else {
        Ok(None)
    }
}

/// Takes a [`DataTransactionHeader`] and caches its `data_root` and tx.id in a
/// cache database table ([`CachedDataRoots`]). Tracks all the tx.ids' that share the same `data_root`.
pub fn cache_data_root<T: DbTx + DbTxMut>(
    tx: &T,
    tx_header: &DataTransactionHeader,
    block_header: Option<&IrysBlockHeader>,
) -> eyre::Result<Option<CachedDataRoot>> {
    let key = tx_header.data_root;

    // Access the current cached entry from the database
    let result = tx.get::<CachedDataRoots>(key)?;

    // Create or update the CachedDataRoot
    let mut cached_data_root = match result {
        Some(existing) => existing,
        None => CachedDataRoot {
            data_size: tx_header.data_size,
            data_size_confirmed: false,
            txid_set: vec![tx_header.id],
            block_set: vec![],
            expiry_height: None,
            cached_at: UnixTimestamp::now()
                .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?,
        },
    };

    // If the entry exists, update the timestamp and add the txid if necessary
    if !cached_data_root.txid_set.contains(&tx_header.id) {
        cached_data_root.txid_set.push(tx_header.id);
    }

    // If the data_size is not yet confirmed and the tx_headers data_size is larger than the one in the cache, update it
    if cached_data_root.data_size < tx_header.data_size && !cached_data_root.data_size_confirmed {
        cached_data_root.data_size = tx_header.data_size;
    }

    // If the entry exists and a block header reference was provided, add the block hash reference if necessary
    if let Some(block_header) = block_header {
        if !cached_data_root
            .block_set
            .contains(&block_header.block_hash)
        {
            cached_data_root.block_set.push(block_header.block_hash);
        }
        // Clear any pre-confirmation expiry once the data_root is included in a block
        cached_data_root.expiry_height = None;
    }

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

/// Caches a [`UnpackedChunk`] - returns `true` if the chunk was a duplicate (present in [`CachedChunks`])
/// and was not inserted into [`CachedChunksIndex`] or [`CachedChunks`]
/// This function ensures that the DataRoot exists in CachedDataRoots before storing the chunk.
pub fn cache_chunk<T: DbTx + DbTxMut>(tx: &T, chunk: &UnpackedChunk) -> eyre::Result<IsDuplicate> {
    let data_root = chunk.data_root;
    // Check if the data root exists
    if tx.get::<CachedDataRoots>(data_root)?.is_none() {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            data_root
        ));
    }

    let chunk_path_hash: ChunkPathHash = chunk.chunk_path_hash();
    if cached_chunk_by_chunk_path_hash(tx, &chunk_path_hash)?.is_some() {
        warn!(
            "Chunk {} of {} is already cached, skipping..",
            &chunk_path_hash, &data_root
        );
        return Ok(true);
    }
    let value = CachedChunkIndexEntry {
        index: chunk.tx_offset,
        meta: CachedChunkIndexMetadata {
            chunk_path_hash,
            updated_at: UnixTimestamp::now()
                .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?,
        },
    };

    debug!(
        "Caching chunk {} ({}) of {}",
        &chunk.tx_offset, &chunk_path_hash, &data_root
    );

    tx.put::<CachedChunksIndex>(data_root, value)?;
    tx.put::<CachedChunks>(chunk_path_hash, chunk.into())?;
    Ok(false)
}

/// Retrieves a cached chunk ([`CachedChunkIndexMetadata`]) from the [`CachedChunksIndex`] using its parent [`DataRoot`] and [`TxChunkOffset`]
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
/// Retrieves a cached chunk ([`(CachedChunkIndexMetadata, CachedChunk)`]) from the cache ([`CachedChunks`] and [`CachedChunksIndex`]) using its parent  [`DataRoot`] and [`TxChunkOffset`]
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
        // the cached chunk always has an entry if the index entry exists
        Ok(Some((
            meta.clone(),
            tx.get::<CachedChunks>(meta.chunk_path_hash)?
                .ok_or_else(|| eyre::eyre!("Chunk has an index entry but no data entry"))?,
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

/// Deletes [`CachedChunk`]s from [`CachedChunks`] by looking up the [`ChunkPathHash`] in [`CachedChunksIndex`]
/// It also removes the index values
pub fn delete_cached_chunks_by_data_root_older_than<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    older_than: UnixTimestamp,
) -> eyre::Result<u64> {
    let mut chunks_pruned = 0;
    // get all chunks specified by the `CachedChunksIndex`
    let mut cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut walker = cursor.walk_dup(Some(data_root), None)?; // iterate a specific key's subkeys
    while let Some((_k, c)) = walker.next().transpose()? {
        if c.meta.updated_at >= older_than {
            continue;
        }
        // delete them
        tx.delete::<CachedChunks>(c.meta.chunk_path_hash, None)?;
        // delete the specific index entry instead of nuking the whole key
        tx.delete::<CachedChunksIndex>(data_root, Some(c))?;
        chunks_pruned += 1;
    }
    // If we removed all subkeys, remove the empty key bucket
    let mut check_cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut remaining = check_cursor.walk_dup(Some(data_root), None)?;
    if remaining.next().transpose()?.is_none() {
        tx.delete::<CachedChunksIndex>(data_root, None)?;
    }
    Ok(chunks_pruned)
}

pub fn get_cache_size<T: Table, TX: DbTx>(tx: &TX, chunk_size: u64) -> eyre::Result<(u64, u64)> {
    let chunk_count: usize = tx.entries::<T>()?;
    let chunk_count_u64 = u64::try_from(chunk_count)
        .map_err(|_| eyre::eyre!("Cache size chunk_count does not fit into u64"))?;
    let total_size = chunk_count_u64
        .checked_mul(chunk_size)
        .ok_or_else(|| eyre::eyre!("Cache size calculation overflow"))?;
    Ok((chunk_count_u64, total_size))
}

pub fn insert_peer_list_item<T: DbTxMut>(
    tx: &T,
    peer_id: &IrysPeerId,
    peer_list_entry: &PeerListItem,
) -> eyre::Result<()> {
    // Convert PeerListItem to PeerListItemInner for database storage
    let inner = peer_list_entry.to_inner();

    // Validate that the peer_id in the payload matches the supplied peer_id
    if peer_list_entry.peer_id != *peer_id {
        eyre::bail!(
            "Peer ID mismatch: supplied peer_id {:?} does not match PeerListItem.peer_id {:?}",
            peer_id,
            peer_list_entry.peer_id
        );
    }

    Ok(tx.put::<PeerListItems>(*peer_id, inner.into())?)
}

/// Gets all ingress proofs associated with a specific data_root
///
pub fn ingress_proofs_by_data_root<TX: DbTx>(
    read_tx: &TX,
    data_root: DataRoot,
) -> eyre::Result<Vec<(DataRoot, CompactCachedIngressProof)>> {
    let mut cursor = read_tx.cursor_dup_read::<IngressProofs>()?;
    let walker = cursor.walk_dup(Some(data_root), None)?; // iterate over all subkeys
    let proofs: Vec<(irys_types::H256, CompactCachedIngressProof)> =
        walker.collect::<Result<Vec<_>, DatabaseError>>()?;

    Ok(proofs)
}

pub fn ingress_proof_by_data_root_address<TX: DbTx>(
    read_tx: &TX,
    data_root: DataRoot,
    address: IrysAddress,
) -> eyre::Result<Option<CompactCachedIngressProof>> {
    let mut cursor = read_tx.cursor_dup_read::<IngressProofs>()?;

    if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, address)?
        .filter(|e| e.address == address)
    {
        Ok(Some(index_entry))
    } else {
        Ok(None)
    }
}

pub fn delete_ingress_proof<T: DbTxMut>(tx: &T, data_root: DataRoot) -> eyre::Result<bool> {
    Ok(tx.delete::<IngressProofs>(data_root, None)?)
}

pub fn store_ingress_proof(
    db: &DatabaseProvider,
    ingress_proof: &IngressProof,
    signer: &IrysSigner,
) -> eyre::Result<()> {
    db.update(|rw_tx| store_ingress_proof_checked(rw_tx, ingress_proof, signer))?
}

pub fn store_ingress_proof_checked<T: DbTx + DbTxMut>(
    tx: &T,
    ingress_proof: &IngressProof,
    signer: &IrysSigner,
) -> eyre::Result<()> {
    if tx
        .get::<CachedDataRoots>(ingress_proof.data_root)?
        .is_none()
    {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            ingress_proof.data_root
        ));
    }

    // Delete all existing proofs for this signer before inserting, as DupSort
    // tables don't upsert — re-anchoring would otherwise produce duplicates.
    let address = signer.address();
    for (_, existing) in ingress_proofs_by_data_root(tx, ingress_proof.data_root)?
        .into_iter()
        .filter(|(_, proof)| proof.address == address)
    {
        tx.delete::<IngressProofs>(ingress_proof.data_root, Some(existing))?;
    }

    tx.put::<IngressProofs>(
        ingress_proof.data_root,
        CompactCachedIngressProof(CachedIngressProof {
            address,
            proof: ingress_proof.clone(),
        }),
    )?;
    Ok(())
}

pub fn store_external_ingress_proof_checked<T: DbTx + DbTxMut>(
    tx: &T,
    ingress_proof: &IngressProof,
    address: IrysAddress,
) -> eyre::Result<()> {
    if tx
        .get::<CachedDataRoots>(ingress_proof.data_root)?
        .is_none()
    {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            ingress_proof.data_root
        ));
    }

    // Delete all existing proofs for this address before inserting (see store_ingress_proof_checked).
    for (_, existing) in ingress_proofs_by_data_root(tx, ingress_proof.data_root)?
        .into_iter()
        .filter(|(_, proof)| proof.address == address)
    {
        tx.delete::<IngressProofs>(ingress_proof.data_root, Some(existing))?;
    }

    tx.put::<IngressProofs>(
        ingress_proof.data_root,
        CompactCachedIngressProof(CachedIngressProof {
            address,
            proof: ingress_proof.clone(),
        }),
    )?;
    Ok(())
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

pub fn database_schema_version<T: DbTx>(tx: &mut T) -> Result<Option<u32>, DatabaseError> {
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

pub fn get_peer_id<T: DbTx>(tx: &T) -> Result<Option<IrysPeerId>, DatabaseError> {
    if let Some(bytes) = tx.get::<Metadata>(MetadataKey::PeerId)? {
        let arr: [u8; 20] = bytes.as_slice().try_into().map_err(|_| {
            DatabaseError::Other("PeerId metadata does not have exactly 20 bytes".to_string())
        })?;

        Ok(Some(IrysPeerId::from(arr)))
    } else {
        Ok(None)
    }
}

pub fn set_peer_id<T: DbTxMut>(tx: &T, peer_id: IrysPeerId) -> Result<(), DatabaseError> {
    let bytes: [u8; 20] = peer_id.into();
    tx.put::<Metadata>(MetadataKey::PeerId, bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use arbitrary::Arbitrary as _;
    use irys_types::{CommitmentTransaction, DataTransactionHeader, IrysBlockHeader, H256};
    use rand::Rng as _;
    use reth_db::Database as _;
    use tempfile::tempdir;

    use crate::{
        block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _,
        insert_commitment_tx, tables::IrysTables,
    };

    use super::{insert_block_header, insert_tx_header, open_or_create_db, tx_header_by_txid};

    #[test]
    fn insert_and_get_tests() -> eyre::Result<()> {
        let path = tempdir()?;
        println!("TempDir: {:?}", path);

        // Generate arbitrary metadata using Arbitrary trait with properly sized buffer
        let mut rng = rand::thread_rng();
        let (min, max) = irys_types::DataTransactionMetadata::size_hint(0);
        let length = max.unwrap_or(min.saturating_mul(4).max(256));
        let bytes: Vec<u8> = (0..length).map(|_| rng.gen()).collect();
        let mut u = arbitrary::Unstructured::new(&bytes);

        let tx_header =
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: irys_types::DataTransactionHeaderV1::default(),
                metadata: irys_types::DataTransactionMetadata::arbitrary(&mut u)?,
            });

        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();

        // Write a Tx
        let _ = db.update(|tx| insert_tx_header(tx, &tx_header))?;

        // Read a Tx
        let result = db.view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?;
        let result_as_v1 = result
            .as_ref()
            .and_then(|h| h.try_as_header_v1().cloned())
            .unwrap();
        assert_eq!(result_as_v1, tx_header.try_as_header_v1().cloned().unwrap());

        // Write a commitment tx
        let commitment_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: irys_types::CommitmentTransactionV2 {
                // Override some defaults to insure deserialization is working
                id: H256::from([10_u8; 32]),
                ..Default::default()
            },
            metadata: Default::default(),
        });
        let _ = db.update(|tx| insert_commitment_tx(tx, &commitment_tx))?;

        // Read a commitment tx
        let result = db.view_eyre(|tx| commitment_tx_by_txid(tx, &commitment_tx.id()))?;
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
}
