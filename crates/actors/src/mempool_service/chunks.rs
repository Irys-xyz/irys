use crate::mempool_service::Inner;
use eyre::eyre;
use irys_database::{
    db::{IrysDatabaseExt as _, IrysDupCursorExt as _},
    db_cache::{data_size_to_chunk_count, DataRootLRUEntry},
    submodule::get_data_size_by_data_root,
    tables::{CachedChunks, CachedChunksIndex, DataRootLRU, IngressProofs},
};
use irys_types::{
    chunk::UnpackedChunk, hash_sha256, irys::IrysSigner, validate_path, DataRoot, DatabaseProvider,
    GossipBroadcastMessage, H256,
};
use lru::LruCache;
use reth::revm::primitives::alloy_primitives::ChainId;
use reth_db::{
    cursor::DbDupCursorRO as _, transaction::DbTx as _, transaction::DbTxMut as _, Database as _,
};
use std::{collections::HashSet, fmt::Display, num::NonZeroUsize};
use tracing::{debug, error, info, warn};

impl Inner {
    pub async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        let mempool_state = &self.mempool_state;
        // TODO: maintain a shared read transaction so we have read isolation
        let max_chunks_per_item = self.config.consensus.mempool.max_chunks_per_item;

        info!(data_root = ?chunk.data_root, number = ?chunk.tx_offset, "Processing chunk");

        // Check to see if we have a cached data_root for this chunk
        let read_tx = self
            .read_tx()
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        let binding = self.storage_modules_guard.read().clone();
        let candidate_sms = binding
            .iter()
            .filter_map(|sm| {
                sm.get_writeable_offsets(&chunk)
                    .ok()
                    .map(|write_offsets| (sm, write_offsets))
            })
            .collect::<Vec<_>>();

        let data_size = irys_database::cached_data_root_by_data_root(&read_tx, chunk.data_root)
            .map_err(|_| ChunkIngressError::DatabaseError)?
            .map(|cdr| cdr.data_size)
            .or_else(|| {
                debug!(data_root=?chunk.data_root, number=?chunk.tx_offset,"Checking SMs for data_size");
                candidate_sms.iter().find_map(|(sm, write_offsets)| {
                    write_offsets.iter().find_map(|wo| {
                        sm.query_submodule_db_by_offset(*wo, |tx| {
                            get_data_size_by_data_root(tx, chunk.data_root)
                        })
                        .ok()
                        .flatten()
                    })
                })
            });

        let data_size = match data_size {
            Some(ds) => ds,
            None => {
                let mut mempool_state_write_guard = mempool_state.write().await;
                // We don't have a data_root for this chunk but possibly the transaction containing this
                // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                if let Some(chunks_map) = mempool_state_write_guard
                    .pending_chunks
                    .get_mut(&chunk.data_root)
                {
                    chunks_map.put(chunk.tx_offset, chunk.clone());
                } else {
                    // If there's no entry for this data_root yet, create one
                    let mut new_lru_cache = LruCache::new(
                        NonZeroUsize::new(max_chunks_per_item)
                            .expect("expected valid NonZeroUsize::new"),
                    );
                    new_lru_cache.put(chunk.tx_offset, chunk.clone());
                    mempool_state_write_guard
                        .pending_chunks
                        .put(chunk.data_root, new_lru_cache);
                }
                drop(mempool_state_write_guard);
                return Ok(());
            }
        };

        // Validate that the data_size for this chunk matches the data_size
        // recorded in the transaction header.
        if data_size != chunk.data_size {
            error!(
                "Error: {:?}. Invalid data_size for data_root: expected: {} got:{}",
                ChunkIngressError::InvalidDataSize,
                data_size,
                chunk.data_size
            );
            return Err(ChunkIngressError::InvalidDataSize);
        }

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = u128::from(chunk.end_byte_offset(self.config.consensus.chunk_size));
        let path_buff = &chunk.data_path;

        info!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let path_result = match validate_path(root_hash, path_buff, target_offset)
            .map_err(|_| ChunkIngressError::InvalidProof)
        {
            Err(e) => {
                error!("error validating path: {:?}", e);
                return Err(e);
            }
            Ok(v) => v,
        };

        // Use data_size to identify and validate that only the last chunk
        // can be less than chunk_size
        let chunk_len = chunk.bytes.len() as u64;

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size
        // Someone may have created a data_root that seemed valid, but if the
        // data_path is valid but the chunk size doesn't mach the protocols
        // consensus size, then the data_root is actually invalid and no future
        // chunks from that data_root should be ingressed.
        let chunk_size = self.config.consensus.chunk_size;

        // Is this chunk index any of the chunks before the last in the tx?
        let num_chunks_in_tx = data_size.div_ceil(chunk_size);
        if u64::from(*chunk.tx_offset) < num_chunks_in_tx - 1 {
            // Ensure prefix chunks are all exactly chunk_size
            if chunk_len != chunk_size {
                error!(
                    "{:?}: incomplete not last chunk, tx offset: {} chunk len: {}",
                    ChunkIngressError::InvalidChunkSize,
                    chunk.tx_offset,
                    chunk_len
                );
                return Ok(());
            }
        } else {
            // Ensure the last chunk is no larger than chunk_size
            if chunk_len > chunk_size {
                error!(
                    "{:?}: chunk bigger than max. chunk size, tx offset: {} chunk len: {}",
                    ChunkIngressError::InvalidChunkSize,
                    chunk.tx_offset,
                    chunk_len
                );
                return Ok(());
            }
        }

        // Check that the leaf hash on the data_path matches the chunk_hash
        match hash_sha256(&chunk.bytes.0).map_err(|_| ChunkIngressError::InvalidDataHash) {
            Err(e) => {
                error!("{:?}: hashed chunk_bytes hash_sha256() errored!", e);
                return Err(e);
            }
            Ok(hash_256) => {
                if path_result.leaf_hash != hash_256 {
                    warn!(
                        "{:?}: leaf_hash does not match hashed chunk_bytes",
                        ChunkIngressError::InvalidDataHash,
                    );
                    return Err(ChunkIngressError::InvalidDataHash);
                }
            }
        }

        // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted
        if let Err(e) = self
            .irys_db
            .update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
            .map_err(|_| ChunkIngressError::DatabaseError)
        {
            error!("Database error: {:?}", e);
            return Err(e);
        }

        for sm in self.storage_modules_guard.read().iter() {
            if !sm
                .get_writeable_offsets(&chunk)
                .unwrap_or_default()
                .is_empty()
            {
                info!(target: "irys::mempool::chunk_ingress", "Writing chunk with offset {} for data_root {} to sm {}", &chunk.tx_offset, &chunk.data_root, &sm.id );
                let result = sm
                    .write_data_chunk(&chunk)
                    .map_err(|_| ChunkIngressError::Other("Internal error".to_owned()));
                if let Err(e) = result {
                    error!("Internal error: {:?}", e);
                    return Err(e);
                }
            }
        }

        // ==== INGRESS PROOFS ====
        let root_hash: H256 = root_hash.into();

        // check if we have generated an ingress proof for this tx already
        // if we have, update it's expiry height

        //  TODO: hook into whatever manages ingress proofs
        match read_tx
            .get::<IngressProofs>(root_hash)
            .map_err(|_| ChunkIngressError::DatabaseError)
        {
            Err(e) => {
                error!("Database error: {:?}", e);
                return Err(e);
            }
            Ok(v) => {
                if v.is_some() {
                    info!(
                        "We've already generated an ingress proof for data root {}",
                        &root_hash
                    );

                    return Ok(());
                };
            }
        }

        // check if we have all the chunks for this tx
        let read_tx = self
            .read_tx()
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        let mut cursor = read_tx
            .cursor_dup_read::<CachedChunksIndex>()
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        // get the number of dupsort values (aka the number of chunks)
        // this ASSUMES that the index isn't corrupt (no double values etc)
        // the ingress proof generation task does a more thorough check
        let chunk_count = cursor
            .dup_count(root_hash)
            .map_err(|_| ChunkIngressError::DatabaseError)?
            .ok_or(ChunkIngressError::DatabaseError)?;

        // data size is the offset of the last chunk
        // add one as index is 0-indexed
        let expected_chunk_count = data_size_to_chunk_count(data_size, chunk_size).unwrap();

        if chunk_count == expected_chunk_count {
            // we *should* have all the chunks
            // dispatch a ingress proof task

            let canon_chain = self.block_tree_read_guard.read().get_canonical_chain();

            let latest = canon_chain
                .0
                .last()
                .ok_or(ChunkIngressError::ServiceUninitialized)
                .unwrap();

            let db = self.irys_db.clone();
            let signer = self.config.irys_signer();
            let latest_height = latest.height;
            let chain_id = self.config.consensus.chain_id;
            self.exec.clone().spawn_blocking(async move {
                generate_ingress_proof(
                    db.clone(),
                    root_hash,
                    data_size,
                    chunk_size,
                    signer,
                    chain_id,
                )
                // TODO: handle results instead of unwrapping
                .unwrap();
                db.update(|wtx| {
                    wtx.put::<DataRootLRU>(
                        root_hash,
                        DataRootLRUEntry {
                            last_height: latest_height,
                            ingress_proof: true,
                        },
                    )
                })
                .unwrap()
                .unwrap();
            });
        }

        let gossip_sender = &self.service_senders.gossip_broadcast.clone();
        let gossip_broadcast_message = GossipBroadcastMessage::from(chunk);

        if let Err(error) = gossip_sender.send(gossip_broadcast_message) {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }

        Ok(())
    }
}

/// Reasons why Chunk Ingress might fail
#[derive(Debug, Clone)]
pub enum ChunkIngressError {
    /// The `data_path/proof` provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
    /// This chunk is for an unknown transaction
    UnknownTransaction,
    /// Only the last chunk in a `data_root` tree can be less than `CHUNK_SIZE`
    InvalidChunkSize,
    /// Chunks should have the same data_size field as their parent tx
    InvalidDataSize,
    /// Some database error occurred when reading or writing the chunk
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    Other(String),
}

impl ChunkIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

/// Generates an ingress proof for a specific `data_root`
/// pulls required data from all sources
pub fn generate_ingress_proof(
    db: DatabaseProvider,
    data_root: DataRoot,
    size: u64,
    chunk_size: u64,
    signer: IrysSigner,
    chain_id: ChainId,
) -> eyre::Result<()> {
    // load the chunks from the DB
    // TODO: for now we assume the chunks all all in the DB chunk cache
    // in future, we'll need access to whatever unified storage provider API we have to get chunks
    // regardless of actual location

    let ro_tx = db.tx()?;
    let mut dup_cursor = ro_tx.cursor_dup_read::<CachedChunksIndex>()?;

    // start from first duplicate entry for this root_hash
    let dup_walker = dup_cursor.walk_dup(Some(data_root), None)?;

    // we need to validate that the index is valid
    // we do this by constructing a set over the chunk hashes, checking if we've seen this hash before
    // if we have, we *must* error
    let mut set = HashSet::<H256>::new();
    let expected_chunk_count = data_size_to_chunk_count(size, chunk_size).unwrap();

    let mut chunk_count: u32 = 0;
    let mut data_size: u64 = 0;

    let iter = dup_walker.into_iter().map(|entry| {
        let (root_hash2, index_entry) = entry?;
        // make sure we haven't traversed into the wrong key
        assert_eq!(data_root, root_hash2);

        let chunk_path_hash = index_entry.meta.chunk_path_hash;
        if set.contains(&chunk_path_hash) {
            return Err(eyre!(
                "Chunk with hash {} has been found twice for index entry {} of data_root {}",
                &chunk_path_hash,
                &index_entry.index,
                &data_root
            ));
        }
        set.insert(chunk_path_hash);

        // TODO: add code to read from ChunkProvider once it can read through CachedChunks & we have a nice system for unpacking chunks on-demand
        let chunk = ro_tx
            .get::<CachedChunks>(index_entry.meta.chunk_path_hash)?
            .ok_or(eyre!(
                "unable to get chunk {chunk_path_hash} for data root {data_root} from DB"
            ))?;

        let chunk_bin = chunk
            .chunk
            .ok_or(eyre!(
                "Missing required chunk ({chunk_path_hash}) body for data root {data_root} from DB"
            ))?
            .0;
        data_size += chunk_bin.len() as u64;
        chunk_count += 1;

        Ok(chunk_bin)
    });

    // generate the ingress proof hash
    let proof = irys_types::ingress::generate_ingress_proof(signer, data_root, iter, chain_id)?;
    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
    );
    assert_eq!(data_size, size);
    assert_eq!({ chunk_count }, expected_chunk_count);

    ro_tx.commit()?;

    db.update(|rw_tx| rw_tx.put::<IngressProofs>(data_root, proof))??;

    Ok(())
}
