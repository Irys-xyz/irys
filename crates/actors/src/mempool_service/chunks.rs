use crate::mempool_service::ingress_proofs::generate_and_store_ingress_proof;
use crate::mempool_service::Inner;
use eyre::eyre;
use irys_database::{
    confirm_data_size_for_data_root,
    db::{IrysDatabaseExt as _, IrysDupCursorExt as _},
    db_cache::data_size_to_chunk_count,
    tables::{CachedChunks, CachedChunksIndex},
};
use irys_types::{
    chunk::UnpackedChunk, hash_sha256, irys::IrysSigner, validate_path, DataRoot, DatabaseProvider,
    GossipBroadcastMessage, IngressProof, H256,
};
use reth::revm::primitives::alloy_primitives::ChainId;
use reth_db::{cursor::DbDupCursorRO as _, transaction::DbTx as _, Database as _};
use std::{collections::HashSet, fmt::Display};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

impl Inner {
    #[instrument(level = "trace", skip_all, err(Debug), fields(chunk.data_root = ?chunk.data_root, chunk.tx_offset = ?chunk.tx_offset))]
    pub async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        let mempool_state = &self.mempool_state;
        // TODO: maintain a shared read transaction so we have read isolation
        let max_chunks_per_item = self.config.node_config.mempool().max_chunks_per_item;

        info!("Processing chunk");

        // Early exit if we've already processed this chunk recently
        let chunk_path_hash = chunk.chunk_path_hash();

        if mempool_state
            .is_a_recent_valid_chunk(&chunk_path_hash)
            .await
        {
            debug!(
                "Chunk {} already processed recently, skipping re-gossip",
                &chunk_path_hash
            );
            return Ok(());
        }

        // Check to see if we have a cached data_root for this chunk
        let cached_data_root = self
            .irys_db
            .view_eyre(|read_tx| {
                irys_database::cached_data_root_by_data_root(read_tx, chunk.data_root)
            })
            .map_err(|e| {
                error!(
                    "Database error fetching cached data_root for chunk data_root {:?} tx_offset {}: {:?}",
                    chunk.data_root, chunk.tx_offset, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        // Extract data_size, falling back to storage modules if not in cache
        let (data_size, data_size_confirmed) = match cached_data_root {
            Some(cdr) => (Some(cdr.data_size), cdr.data_size_confirmed),
            None => {
                debug!(
                    chunk.data_root = ?chunk.data_root,
                    chunk.tx_offset = ?chunk.tx_offset,
                    "Checking SMs for data_size"
                );
                // Get a list of all the local storage modules
                let storage_modules = self.storage_modules_guard.read().clone();

                // Iterate the modules - collecting and filtering any DataRootInfos for the maximum data_size
                let sm_data_size = storage_modules
                    .iter()
                    .filter_map(|sm| {
                        sm.collect_data_root_infos(chunk.data_root)
                            .ok()
                            .filter(|mi| !mi.0.is_empty()) // Remove empty MetadataIndex entries
                    })
                    .flat_map(|m| m.0.iter().map(|md| md.data_size).collect::<Vec<_>>())
                    .max();
                (sm_data_size, true)
            }
        };

        let data_size = match data_size {
            Some(ds) => ds,
            None => {
                // We don't have a data_root for this chunk but possibly the transaction containing this
                // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                // Pre-header sanity checks to reduce DoS risk.
                let chunk_size = self.config.consensus.chunk_size;
                let chunk_len_u64 = u64::try_from(chunk.bytes.len())
                    .map_err(|_| AdvisoryChunkIngressError::PreHeaderOversizedBytes)?;
                if chunk_len_u64 > chunk_size {
                    warn!(
                        "Dropping pre-header chunk for {} at offset {}: bytes.len() {} exceeds chunk_size {}",
                        &chunk.data_root,
                        &chunk.tx_offset,
                        chunk.bytes.len(),
                        chunk_size
                    );
                    return Err(AdvisoryChunkIngressError::PreHeaderOversizedBytes.into());
                }
                let preheader_data_path_max_bytes =
                    self.config.mempool.max_preheader_data_path_bytes;
                let preheader_chunks_per_item_cap =
                    self.config.mempool.max_preheader_chunks_per_item;
                if chunk.data_path.0.len() > preheader_data_path_max_bytes {
                    warn!(
                        "Dropping pre-header chunk for {} at offset {}: data_path too large ({} > {})",
                        &chunk.data_root,
                        &chunk.tx_offset,
                        chunk.data_path.0.len(),
                        preheader_data_path_max_bytes
                    );
                    return Err(AdvisoryChunkIngressError::PreHeaderOversizedDataPath.into());
                }
                let preheader_chunks_per_item =
                    std::cmp::min(max_chunks_per_item, preheader_chunks_per_item_cap);
                if usize::try_from(*chunk.tx_offset).unwrap_or(usize::MAX)
                    >= preheader_chunks_per_item
                {
                    warn!(
                        "Dropping pre-header chunk for {} at offset {}: tx_offset {} exceeds pre-header capacity {}",
                        &chunk.data_root,
                        &chunk.tx_offset,
                        *chunk.tx_offset,
                        preheader_chunks_per_item
                    );
                    return Err(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap.into());
                }

                self.mempool_state.put_chunk(chunk.clone()).await;
                return Ok(());
            }
        };

        debug!("Got data root and data size");
        // Validate that the data_size for this chunk is less than or equal to
        // the largest data_size paid for by a data tx with this data_root
        if chunk.data_size > data_size {
            if data_size_confirmed {
                // Confirmed size is authoritative - reject mismatched chunks
                error!(
                    "Error: {:?}. Invalid data_size for data_root: expected: {} got:{}",
                    CriticalChunkIngressError::InvalidDataSize,
                    data_size,
                    chunk.data_size
                );
                return Err(CriticalChunkIngressError::InvalidDataSize.into());
            } else {
                // Unconfirmed: chunk claims larger size than cached.
                // This may be legitimate (tx with larger size not yet processed).
                // Park the chunk for the time being.
                debug!(
                    "Chunk claims larger data_size {} than unconfirmed cached {} for data_root {:?}. Parking chunk.",
                    chunk.data_size, data_size, chunk.data_root
                );
                self.mempool_state
                    .put_chunk(chunk.clone(), max_chunks_per_item)
                    .await;
                return Ok(());
            }
        }

        // Validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash

        if data_size == 0 {
            error!(
                "Error: {:?}. Invalid data_size for data_root: {:?}. got 0 bytes",
                CriticalChunkIngressError::InvalidDataSize,
                chunk.data_root,
            );
            return Err(CriticalChunkIngressError::InvalidDataSize.into());
        }

        let root_hash = chunk.data_root.0;
        let target_offset = u128::from(chunk.end_byte_offset(self.config.consensus.chunk_size));
        let path_buff = &chunk.data_path;

        info!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let path_result = match validate_path(root_hash, path_buff, target_offset)
            .map_err(|_| CriticalChunkIngressError::InvalidProof)
        {
            Err(e) => {
                error!("error validating path: {:?}", e);
                return Err(e.into());
            }
            Ok(v) => v,
        };

        // Check and see if this is the rightmost chunk
        if path_result.is_rightmost_chunk {
            // If this is the rightmost chunk in the data_root we can use it to
            // validate the data_size and mark it as "confirmed" in the cache.
            //
            // In this case the data path stores offsets so we need to add one
            // to get the size in bytes.
            let confirmed_data_size: u64 = path_result
                .max_byte_range
                .try_into()
                .expect("to convert U128 path_result.max_byte_range to data_size to u64");

            self.irys_db
                .update_eyre(|db_tx| {
                    confirm_data_size_for_data_root(db_tx, &chunk.data_root, confirmed_data_size)
                })
                .expect("confirm_data_size database operation to succeed");
        }

        // Use data_size to identify and validate that only the last chunk
        // can be less than chunk_size
        let chunk_len = u64::try_from(chunk.bytes.len())
            .map_err(|_| CriticalChunkIngressError::InvalidChunkSize)?;

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size
        // Someone may have created a data_root that seemed valid, but if the
        // data_path is valid but the chunk size doesn't mach the protocols
        // consensus size, then the data_root is actually invalid and no future
        // chunks from that data_root should be ingressed.
        let chunk_size = self.config.consensus.chunk_size;

        // Validate that we will have chunks in the tx
        let num_chunks_in_tx = data_size.div_ceil(chunk_size);
        if num_chunks_in_tx == 0 {
            error!(
                "Error: {:?}. Invalid data_size for data_root: {:?}",
                CriticalChunkIngressError::InvalidDataSize,
                chunk.data_root,
            );
            return Err(CriticalChunkIngressError::InvalidDataSize.into());
        }
        // Is this chunk index any of the chunks before the last in the tx?
        let last_index = num_chunks_in_tx - 1;
        if u64::from(*chunk.tx_offset) < last_index {
            // Ensure prefix chunks are all exactly chunk_size
            if chunk_len != chunk_size {
                error!(
                    "{:?}: incomplete not last chunk, tx offset: {} chunk len: {}",
                    CriticalChunkIngressError::InvalidChunkSize,
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
                    CriticalChunkIngressError::InvalidChunkSize,
                    chunk.tx_offset,
                    chunk_len
                );
                return Ok(());
            }
        }

        // Check that the leaf hash on the data_path matches the chunk_hash
        let hash_256 = hash_sha256(&chunk.bytes.0);
        if path_result.leaf_hash != hash_256 {
            warn!(
                "{:?}: leaf_hash does not match hashed chunk_bytes",
                CriticalChunkIngressError::InvalidDataHash,
            );
            return Err(CriticalChunkIngressError::InvalidDataHash.into());
        }

        // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted
        if let Err(e) = self
            .irys_db
            .update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
            .map_err(|e| {
                error!(
                    "Database error caching chunk data_root {:?} tx_offset {}: {:?}",
                    chunk.data_root, chunk.tx_offset, e
                );
                CriticalChunkIngressError::DatabaseError
            })
        {
            return Err(e.into());
        }

        // Add to recent valid chunks cache to prevent re-processing
        mempool_state
            .record_recent_valid_chunk(chunk_path_hash)
            .await;

        // Write chunk to storage modules that have writeable offsets for this chunk.
        // Note: get_writeable_offsets() only returns offsets within the data_size
        // bounds for the data_root stored at that location in the storage module.
        for sm in self.storage_modules_guard.read().iter() {
            if !sm
                .get_writeable_offsets(&chunk)
                .unwrap_or_default()
                .is_empty()
            {
                info!(target: "irys::mempool::chunk_ingress", "Writing chunk with offset {} for data_root {} to sm {}", &chunk.tx_offset, &chunk.data_root, &sm.id );
                let result = sm
                    .write_data_chunk(&chunk)
                    .map_err(|e| {
                        error!(
                            "Failed to write chunk data_root {:?} tx_offset {} to storage_module {}: {:?}",
                            chunk.data_root, chunk.tx_offset, sm.id, e
                        );
                        CriticalChunkIngressError::Other(format!(
                            "Failed to write chunk to storage_module {}", sm.id
                        ))
                    });
                if let Err(e) = result {
                    return Err(e.into());
                }
            }
        }

        // Gossip the chunk before moving onto ingress proof checks
        let chunk_data_root = chunk.data_root;
        let chunk_tx_offset = chunk.tx_offset;
        let gossip_broadcast_message = GossipBroadcastMessage::from(chunk);

        if let Err(error) = self
            .service_senders
            .gossip_broadcast
            .send(gossip_broadcast_message)
        {
            tracing::error!(
                "Failed to send gossip data for chunk data_root {:?} tx_offset {}: {:?}",
                chunk_data_root,
                chunk_tx_offset,
                error
            );
        }

        // ==== INGRESS PROOFS ====
        let root_hash: H256 = root_hash.into();
        let cached_data_root = self
            .irys_db
            .view_eyre(|read_tx| irys_database::cached_data_root_by_data_root(read_tx, root_hash))
            .map_err(|_| CriticalChunkIngressError::DatabaseError)?;

        // Early out: only generate ingress proofs for confirmed data sizes
        let Some(cdr) = cached_data_root else {
            return Ok(());
        };

        if !cdr.data_size_confirmed {
            return Ok(());
        }

        // Be explicit that data_size used from here on is the confirmed data size
        let data_size = cdr.data_size;

        // check if we have generated an ingress proof for this tx already
        // if we have, update it's expiry height

        // Determine existing proof state and chunk count
        let (chunk_count_opt, existing_local_proof) = self
            .irys_db
            .view_eyre(|tx| {
                let existing_local_proof: Option<IngressProof> = None;

                // Count chunks (needed for generation & potential regeneration)
                let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
                let count = cursor
                    .dup_count(root_hash)?
                    .ok_or_else(|| eyre::eyre!("No chunks found for data root"))?;
                Ok((Some(count), existing_local_proof))
            })
            .map_err(|e| {
                error!(
                    "Database error checking ingress proof/chunk count for data_root {:?}: {:?}",
                    root_hash, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        // Early return if we have a valid existing local proof
        if chunk_count_opt.is_none() && existing_local_proof.is_some() {
            info!(
                "Local ingress proof already exists and is valid for data root {}",
                &root_hash
            );
            return Ok(());
        }

        let chunk_count =
            chunk_count_opt.expect("chunk_count present when proof missing or expired");

        // Compute expected number of chunks from data_size using ceil(data_size / chunk_size)
        // This equals the last chunk index + 1 (since tx offsets are 0-indexed)
        let Ok(expected_chunk_count) = data_size_to_chunk_count(data_size, chunk_size) else {
            error!(
                "Error: {:?}. Invalid data_size for data_root: {:?}",
                CriticalChunkIngressError::InvalidDataSize,
                chunk_data_root,
            );
            return Err(CriticalChunkIngressError::InvalidDataSize.into());
        };

        if chunk_count == expected_chunk_count {
            // we *should* have all the chunks
            let db = self.irys_db.clone();
            let block_tree_read_guard = self.block_tree_read_guard.clone();
            let config = self.config.clone();
            let gossip_sender = self.service_senders.gossip_broadcast.clone();
            let cache_sender = self.service_senders.chunk_cache.clone();
            let _fut = self.exec.clone().spawn_blocking(async move {
                if let Err(e) = generate_and_store_ingress_proof(
                    &block_tree_read_guard,
                    &db,
                    &config,
                    chunk_data_root,
                    None,
                    &gossip_sender,
                    &cache_sender,
                ) {
                    tracing::warn!(proof.data_root = ?chunk_data_root, "Failed to generate ingress proof: {e}");
                }
            }).in_current_span();
        }
        Ok(())
    }
}

/// Reasons why Chunk Ingress might fail
#[derive(Debug, Clone, thiserror::Error)]
pub enum ChunkIngressError {
    #[error("Critical chunk ingress error: {0:?}")]
    Critical(CriticalChunkIngressError),
    #[error("Advisory chunk ingress error: {0:?}")]
    Advisory(AdvisoryChunkIngressError),
}

impl ChunkIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>, critical: bool) -> Self {
        if critical {
            Self::Critical(CriticalChunkIngressError::Other(err.into()))
        } else {
            Self::Advisory(AdvisoryChunkIngressError::Other(err.into()))
        }
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display, critical: bool) -> Self {
        if critical {
            Self::Critical(CriticalChunkIngressError::Other(err.to_string()))
        } else {
            Self::Advisory(AdvisoryChunkIngressError::Other(err.to_string()))
        }
    }
}

impl From<CriticalChunkIngressError> for ChunkIngressError {
    fn from(value: CriticalChunkIngressError) -> Self {
        Self::Critical(value)
    }
}

impl From<AdvisoryChunkIngressError> for ChunkIngressError {
    fn from(value: AdvisoryChunkIngressError) -> Self {
        Self::Advisory(value)
    }
}

/// Reasons why Chunk Ingress might fail
#[derive(Debug, Clone)]
pub enum CriticalChunkIngressError {
    /// The `data_path/proof` provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
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

// non-critical reasons why chunk ingress might fail
#[derive(Debug, Clone)]
pub enum AdvisoryChunkIngressError {
    /// Oversized chunk bytes submitted before header arrival
    PreHeaderOversizedBytes,
    /// Oversized data_path submitted before header arrival
    PreHeaderOversizedDataPath,
    /// tx_offset exceeds pre-header capacity bound
    PreHeaderOffsetExceedsCap,
    /// Catch-all variant for other errors.
    Other(String),
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
    anchor: H256,
) -> eyre::Result<IngressProof> {
    // load the chunks from the DB
    // TODO: for now we assume the chunks all all in the DB chunk cache
    // in future, we'll need access to whatever unified storage provider API we have to get chunks
    // regardless of actual location

    let expected_chunk_count = data_size_to_chunk_count(size, chunk_size)?;

    let (proof, actual_data_size, actual_chunk_count) = db.view_eyre(|tx| {
        let mut dup_cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;

        // start from first duplicate entry for this root_hash
        let dup_walker = dup_cursor.walk_dup(Some(data_root), None)?;

        // we need to validate that the index is valid
        // we do this by constructing a set over the chunk hashes, checking if we've seen this hash before
        // if we have, we *must* error
        let mut set = HashSet::<H256>::new();

        let mut chunk_count: u32 = 0;
        let mut total_data_size: u64 = 0;

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
            let chunk = tx
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
            let chunk_len =
                u64::try_from(chunk_bin.len()).map_err(|_| eyre!("chunk length exceeds u64"))?;
            total_data_size += chunk_len;
            chunk_count += 1;

            Ok(chunk_bin)
        });

        // generate the ingress proof hash
        let proof = irys_types::ingress::generate_ingress_proof(
            &signer, data_root, iter, chain_id, anchor,
        )?;

        Ok((proof, total_data_size, chunk_count))
    })?;

    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
    );
    assert_eq!(actual_data_size, size);
    assert_eq!(actual_chunk_count, expected_chunk_count);

    db.update(|rw_tx| irys_database::store_ingress_proof_checked(rw_tx, &proof, &signer))??;

    Ok(proof)
}
