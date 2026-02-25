use super::ChunkIngressServiceInner;
use super::ingress_proofs::generate_and_store_ingress_proof;
use super::metrics::{
    record_chunk_duplicate, record_chunk_ingested, record_enqueue_duration, record_flush_failure,
    record_validation_duration,
};
use eyre::eyre;
use irys_database::{
    confirm_data_size_for_data_root,
    db::{IrysDatabaseExt as _, IrysDupCursorExt as _},
    db_cache::data_size_to_chunk_count,
    tables::{CachedChunks, CachedChunksIndex},
};
use irys_types::gossip::v2::GossipBroadcastMessageV2;
use irys_types::{
    DataLedger, DataRoot, DatabaseProvider, H256, IngressProof, SendTraced as _,
    chunk::{UnpackedChunk, max_chunk_offset},
    hash_sha256,
    irys::IrysSigner,
    validate_path,
};
use irys_utils::ElapsedMs as _;
use rayon::prelude::*;
use reth::revm::primitives::alloy_primitives::ChainId;
use reth_db::{Database as _, cursor::DbDupCursorRO as _, transaction::DbTx as _};
use std::sync::Arc;
use std::time::Instant;
use std::{collections::HashSet, fmt::Display};
use tracing::{Instrument as _, debug, error, info, info_span, instrument, warn};

/// Represents data_size information and if it comes from the publish ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSizeInfo {
    /// The data_size value
    pub data_size: u64,
    /// Whether this data_size is from the publish ledger (trusted)
    pub is_from_publish_ledger: bool,
}

/// Selects the authoritative data_size from multiple storage modules.
#[must_use]
pub fn select_data_size_from_storage_modules(
    storage_module_data: impl IntoIterator<Item = DataSizeInfo>,
) -> Option<DataSizeInfo> {
    let mut submit_ledger_max: Option<u64> = None;
    let mut publish_ledger_max: Option<u64> = None;

    for sm_data in storage_module_data {
        if sm_data.is_from_publish_ledger {
            let current_max = publish_ledger_max.unwrap_or(0);
            if sm_data.data_size > current_max {
                publish_ledger_max = Some(sm_data.data_size);
            }
        } else {
            let current_max = submit_ledger_max.unwrap_or(0);
            if sm_data.data_size > current_max {
                submit_ledger_max = Some(sm_data.data_size);
            }
        }
    }

    // Prefer publish ledger data_size over submit ledger
    if let Some(ds) = publish_ledger_max {
        Some(DataSizeInfo {
            data_size: ds,
            is_from_publish_ledger: true,
        })
    } else {
        submit_ledger_max.map(|ds| DataSizeInfo {
            data_size: ds,
            is_from_publish_ledger: false,
        })
    }
}

impl ChunkIngressServiceInner {
    #[instrument(level = "info", skip_all, err(Debug), fields(chunk.data_root = ?chunk.data_root, chunk.tx_offset = ?chunk.tx_offset))]
    pub(crate) async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        let chunk = Arc::new(chunk);
        // TODO: maintain a shared read transaction so we have read isolation
        let max_chunks_per_item = self.config.node_config.mempool().max_chunks_per_item;
        let chunk_size = self.config.consensus.chunk_size;

        info!(
            chunk.data_root = ?chunk.data_root,
            chunk.tx_offset = %chunk.tx_offset,
            chunk.data_size = chunk.data_size,
            "Processing chunk"
        );

        // Early exit if we've already processed this chunk recently
        let chunk_path_hash = chunk.chunk_path_hash();

        if self
            .recent_valid_chunks
            .read()
            .await
            .contains(&chunk_path_hash)
        {
            debug!(
                "Chunk {} already processed recently, skipping re-gossip",
                &chunk_path_hash
            );
            record_chunk_duplicate();
            return Ok(());
        }

        // Check to see if we have a cached data_root for this chunk
        let _fetch_size_span = info_span!("chunk.fetch_data_size").entered();
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

        let (data_size, data_size_confirmed) = match cached_data_root {
            Some(cdr) => (Some(cdr.data_size), cdr.data_size_confirmed),
            None => {
                debug!(
                    chunk.data_root = ?chunk.data_root,
                    chunk.tx_offset = ?chunk.tx_offset,
                    "Checking SMs for data_size"
                );
                let storage_modules_guard = self.storage_modules_guard.read();

                // Collect data_size info from each storage module in parallel
                let sm_data_sizes: Vec<DataSizeInfo> = storage_modules_guard
                    .par_iter()
                    .filter_map(|sm| {
                        let infos = sm.collect_data_root_infos(chunk.data_root).ok()?;
                        if infos.0.is_empty() {
                            return None;
                        }

                        let is_from_publish_ledger = sm
                            .partition_assignment()
                            .is_some_and(|pa| pa.ledger_id == Some(DataLedger::Publish.get_id()));

                        // Find max data_size within this SM's infos
                        let max_data_size = infos.0.iter().map(|info| info.data_size).max()?;

                        Some(DataSizeInfo {
                            data_size: max_data_size,
                            is_from_publish_ledger,
                        })
                    })
                    .collect();

                match select_data_size_from_storage_modules(sm_data_sizes) {
                    Some(selected) if selected.is_from_publish_ledger => {
                        (Some(selected.data_size), true)
                    }
                    Some(selected) => {
                        // Submit ledger data needs verification
                        let confirmed = self.verify_data_size_from_storage_modules(
                            &storage_modules_guard,
                            chunk.data_root,
                            selected.data_size,
                        );
                        (Some(selected.data_size), confirmed)
                    }
                    None => (None, false),
                }
            }
        };

        drop(_fetch_size_span);

        let data_size = match data_size {
            Some(ds) => ds,
            None => {
                // We don't have a data_root for this chunk but possibly the transaction containing this
                // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                // Pre-header sanity checks to reduce DoS risk.
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

                if !chunk.is_valid_offset(chunk_size) {
                    let max_offset = chunk.max_valid_offset(chunk_size);
                    warn!(
                        "Dropping pre-header chunk for {} at offset {}: tx_offset {} exceeds max valid offset {:?} for data_size {}",
                        &chunk.data_root,
                        &chunk.tx_offset,
                        *chunk.tx_offset,
                        max_offset,
                        chunk.data_size
                    );
                    return Err(AdvisoryChunkIngressError::PreHeaderInvalidOffset(format!(
                        "tx_offset {} exceeds max valid offset {:?} for data_size {}",
                        *chunk.tx_offset, max_offset, chunk.data_size
                    ))
                    .into());
                }

                let preheader_chunks_per_item =
                    std::cmp::min(max_chunks_per_item, preheader_chunks_per_item_cap);

                // Acquire a single write lock so the count check and the insert are
                // atomic with respect to concurrent ingress calls, eliminating the
                // TOCTOU race that could allow the per-item cap to be exceeded.
                let mut pending_chunks_guard = self.pending_chunks.write().await;
                let current_chunk_count = pending_chunks_guard
                    .get(&chunk.data_root)
                    .map(lru::LruCache::len)
                    .unwrap_or(0);
                if current_chunk_count >= preheader_chunks_per_item {
                    warn!(
                        "Dropping pre-header chunk for {} at offset {}: cache full ({}/{})",
                        &chunk.data_root,
                        &chunk.tx_offset,
                        current_chunk_count,
                        preheader_chunks_per_item
                    );
                    return Err(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap.into());
                }

                pending_chunks_guard.put(Arc::unwrap_or_clone(chunk));
                return Ok(());
            }
        };

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
                self.pending_chunks
                    .write()
                    .await
                    .put(Arc::unwrap_or_clone(chunk));
                return Ok(());
            }
        }

        // Validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let _proof_span = info_span!("chunk.validate_proof").entered();

        let Some(max_valid_offset) = max_chunk_offset(data_size, chunk_size) else {
            error!(
                "Error: {:?}. Invalid data_size for data_root: {:?}. got 0 bytes",
                CriticalChunkIngressError::InvalidDataSize,
                chunk.data_root,
            );
            return Err(CriticalChunkIngressError::InvalidDataSize.into());
        };

        let offset_u64 = u64::from(*chunk.tx_offset);

        if offset_u64 > max_valid_offset {
            let num_chunks = data_size.div_ceil(chunk_size);
            error!(
                "Invalid tx_offset: {} exceeds max valid offset {} for data_size {} (num_chunks: {})",
                offset_u64, max_valid_offset, data_size, num_chunks
            );
            return Err(CriticalChunkIngressError::InvalidOffset(format!(
                "tx_offset {} exceeds max valid offset {} for data_size {}",
                offset_u64, max_valid_offset, data_size
            ))
            .into());
        }

        let root_hash = chunk.data_root.0;
        let target_offset = match chunk.end_byte_offset_checked(chunk_size) {
            Some(offset) => u128::from(offset),
            None => {
                error!(
                    "Byte offset calculation failed for tx_offset {} with data_size {}",
                    *chunk.tx_offset, chunk.data_size
                );
                return Err(CriticalChunkIngressError::InvalidOffset(
                    "Byte offset calculation overflow or invalid offset".to_string(),
                )
                .into());
            }
        };
        let path_buff = &chunk.data_path;

        info!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let validation_start = Instant::now();
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

        // Note: num_chunks, chunk_size, offset_u64, and max_valid_offset already
        // computed in offset validation above
        let is_last_chunk = offset_u64 == max_valid_offset;

        if is_last_chunk {
            // Last chunk can be <= chunk_size
            if chunk_len > chunk_size {
                error!(
                    "Last chunk exceeds max size: tx_offset {} chunk_len {} max {}",
                    chunk.tx_offset, chunk_len, chunk_size
                );
                return Ok(());
            }
        } else {
            // Non-last chunk must be exactly chunk_size
            if chunk_len != chunk_size {
                error!(
                    "Non-last chunk has wrong size: tx_offset {} chunk_len {} expected {}",
                    chunk.tx_offset, chunk_len, chunk_size
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

        record_validation_duration(validation_start.elapsed_ms());

        drop(_proof_span);

        // Write-behind: defers MDBX write for throughput
        let storage_start = Instant::now();
        match self
            .chunk_data_writer
            .queue_write(Arc::clone(&chunk))
            .instrument(info_span!("chunk.cache_write"))
            .await
        {
            Ok(true) => {
                record_chunk_duplicate();
            }
            Ok(false) => {}
            Err(e) => {
                error!(
                    "Write-behind queue error for chunk data_root {:?} tx_offset {}: {:?}",
                    chunk.data_root, chunk.tx_offset, e
                );
                return Err(CriticalChunkIngressError::Other(format!(
                    "Write-behind channel closed: {e:?}"
                ))
                .into());
            }
        }
        record_enqueue_duration(storage_start.elapsed_ms());

        // Add to recent valid chunks cache to prevent re-processing
        self.recent_valid_chunks
            .write()
            .await
            .put(chunk_path_hash, ());

        record_chunk_ingested(chunk_len);

        // Write chunk to storage modules that have writeable offsets for this chunk.
        // Note: get_writeable_offsets() only returns offsets within the data_size
        // bounds for the data_root stored at that location in the storage module.
        let _sm_span = info_span!("chunk.write_storage_modules").entered();
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

        drop(_sm_span);

        // Gossip the chunk before moving onto ingress proof checks
        let chunk_data_root = chunk.data_root;
        let chunk_tx_offset = chunk.tx_offset;
        let gossip_broadcast_message = GossipBroadcastMessageV2::from(chunk);

        if let Err(error) = self
            .service_senders
            .gossip_broadcast
            .send_traced(gossip_broadcast_message)
        {
            tracing::error!(
                "Failed to send gossip data for chunk data_root {:?} tx_offset {}: {:?}",
                chunk_data_root,
                chunk_tx_offset,
                error
            );
        }

        // Flush to ensure chunks are committed before ingress proof check.
        if let Err(e) = self.chunk_data_writer.flush().await {
            error!(
                "Failed to flush chunk data writer before ingress proof check: {:?}",
                e
            );
            record_flush_failure();
            return Ok(());
        }

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
        let local_address = self.config.irys_signer().address();
        let (chunk_count, existing_local_proof) = self
            .irys_db
            .view_eyre(|tx| {
                let existing_local_proof = irys_database::ingress_proof_by_data_root_address(
                    tx,
                    root_hash,
                    local_address,
                )?;

                // Count chunks (needed for generation & potential regeneration)
                let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
                let count = cursor
                    .dup_count(root_hash)?
                    .ok_or_else(|| eyre::eyre!("No chunks found for data root"))?;
                Ok((count, existing_local_proof))
            })
            .map_err(|e| {
                error!(
                    "Database error checking ingress proof/chunk count for data_root {:?}: {:?}",
                    root_hash, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        // Early return if we have a valid existing local proof
        if existing_local_proof.is_some() {
            info!(
                "Local ingress proof already exists and is valid for data root {}",
                &root_hash
            );
            return Ok(());
        }

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
                if let Err(error) = generate_and_store_ingress_proof(
                    &block_tree_read_guard,
                    &db,
                    &config,
                    chunk_data_root,
                    None,
                    &gossip_sender,
                    &cache_sender,
                ) {
                    if error.is_benign() {
                        debug!(proof.data_root = ?chunk_data_root, "Skipped ingress proof generation: {error}");
                    } else {
                        warn!(proof.data_root = ?chunk_data_root, "Failed to generate ingress proof: {error}");
                    }
                }
            }).in_current_span();
        }
        Ok(())
    }

    /// Verifies data_size by fetching the expected last chunk and validating its merkle proof.
    fn verify_data_size_from_storage_modules(
        &self,
        storage_modules: &[std::sync::Arc<irys_domain::StorageModule>],
        data_root: DataRoot,
        claimed_data_size: u64,
    ) -> bool {
        let chunk_size = self.config.consensus.chunk_size;
        // Chunk index of the last chunk (0-indexed)
        let last_chunk_index = claimed_data_size.div_ceil(chunk_size).saturating_sub(1);
        // Byte offset for path validation (last byte position)
        let target_byte_offset = u128::from(claimed_data_size.saturating_sub(1));

        // Search storage modules in parallel for a valid rightmost chunk proof.
        let confirmed_size = storage_modules.par_iter().find_map_any(|sm| {
            let infos = sm
                .collect_data_root_infos(data_root)
                .ok()
                .filter(|i| !i.0.is_empty())?;

            for info in &infos.0 {
                let relative_offset =
                    (info.start_offset.0 as i64).saturating_add(last_chunk_index as i64);
                if relative_offset < 0 {
                    continue;
                }

                let partition_offset =
                    irys_types::PartitionChunkOffset::from(relative_offset as u32);

                // Use get_chunk_metadata to avoid reading chunk bytes - we only need the data_path
                let (chunk_data_root, data_path) = sm
                    .get_chunk_metadata(partition_offset)
                    .ok()
                    .flatten()
                    .filter(|(dr, _)| *dr == data_root)?;

                if let Ok(result) = validate_path(chunk_data_root.0, &data_path, target_byte_offset)
                {
                    if result.is_rightmost_chunk {
                        return Some(result.max_byte_range as u64);
                    }
                }
            }
            None
        });

        match confirmed_size {
            Some(size) => {
                debug!(
                    "Verified data_size {} for data_root {:?} via rightmost chunk proof",
                    size, data_root
                );
                // Cache the confirmed data_size so subsequent chunks skip verification
                if let Err(e) = self
                    .irys_db
                    .update_eyre(|db_tx| confirm_data_size_for_data_root(db_tx, &data_root, size))
                {
                    warn!(
                        "Failed to cache confirmed data_size for {:?}: {:?}",
                        data_root, e
                    );
                }
                true
            }
            None => {
                debug!(
                    "Could not verify data_size {} for data_root {:?} - no valid rightmost chunk found",
                    claimed_data_size, data_root
                );
                false
            }
        }
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
    pub fn other(err: impl Into<String>, critical: bool) -> Self {
        if critical {
            Self::Critical(CriticalChunkIngressError::Other(err.into()))
        } else {
            Self::Advisory(AdvisoryChunkIngressError::Other(err.into()))
        }
    }

    pub fn other_display(err: impl Display, critical: bool) -> Self {
        if critical {
            Self::Critical(CriticalChunkIngressError::Other(err.to_string()))
        } else {
            Self::Advisory(AdvisoryChunkIngressError::Other(err.to_string()))
        }
    }

    pub fn is_advisory(&self) -> bool {
        matches!(self, Self::Advisory(_))
    }

    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Critical(e) => e.error_type(),
            Self::Advisory(e) => e.error_type(),
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

#[derive(Debug, Clone)]
pub enum CriticalChunkIngressError {
    InvalidProof,
    InvalidDataHash,
    InvalidChunkSize,
    InvalidDataSize,
    InvalidOffset(String),
    DatabaseError,
    ServiceUninitialized,
    Other(String),
}

impl CriticalChunkIngressError {
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::InvalidProof => "invalid_proof",
            Self::InvalidDataHash => "invalid_data_hash",
            Self::InvalidChunkSize => "invalid_chunk_size",
            Self::InvalidDataSize => "invalid_data_size",
            Self::InvalidOffset(_) => "invalid_offset",
            Self::DatabaseError => "database_error",
            Self::ServiceUninitialized => "service_uninitialized",
            Self::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub enum AdvisoryChunkIngressError {
    PreHeaderOversizedBytes,
    PreHeaderOversizedDataPath,
    PreHeaderOffsetExceedsCap,
    PreHeaderInvalidOffset(String),
    Other(String),
}

impl AdvisoryChunkIngressError {
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::PreHeaderOversizedBytes => "pre_header_oversized_bytes",
            Self::PreHeaderOversizedDataPath => "pre_header_oversized_data_path",
            Self::PreHeaderOffsetExceedsCap => "pre_header_offset_exceeds_cap",
            Self::PreHeaderInvalidOffset(_) => "pre_header_invalid_offset",
            Self::Other(_) => "other",
        }
    }
}

/// Generates an ingress proof for a specific `data_root`
/// pulls required data from all sources
#[must_use = "the generated ingress proof should be used or stored"]
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    /// Helper to create a submit ledger data size entry for test cases
    fn submit(data_size: u64) -> DataSizeInfo {
        DataSizeInfo {
            data_size,
            is_from_publish_ledger: false,
        }
    }

    /// Helper to create a publish ledger data size entry for test cases
    fn publish(data_size: u64) -> DataSizeInfo {
        DataSizeInfo {
            data_size,
            is_from_publish_ledger: true,
        }
    }

    #[rstest]
    #[case::empty(vec![], None)]
    #[case::single_submit(
        vec![submit(1000)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: false })
    )]
    #[case::single_publish(
        vec![publish(1000)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: true })
    )]
    #[case::multiple_submit_returns_max(
        vec![submit(500), submit(1000), submit(750)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: false })
    )]
    #[case::multiple_publish_returns_max(
        vec![publish(500), publish(1000), publish(750)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: true })
    )]
    #[case::submit_only_when_no_publish(
        vec![submit(5000), submit(10000)],
        Some(DataSizeInfo { data_size: 10000, is_from_publish_ledger: false })
    )]
    // These cases test to make sure the publish ledger is preferred over the submit ledger
    #[case::publish_preferred_over_larger_submit(
        vec![submit(10000), publish(1000)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: true })
    )]
    #[case::publish_first_still_preferred(
        vec![publish(1000), submit(10000)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: true })
    )]
    #[case::interleaved_uses_publish_max(
        vec![submit(5000), publish(500), submit(10000), publish(1000), submit(7500)],
        Some(DataSizeInfo { data_size: 1000, is_from_publish_ledger: true })
    )]
    fn select_data_size_cases(
        #[case] entries: Vec<DataSizeInfo>,
        #[case] expected: Option<DataSizeInfo>,
    ) {
        let result = select_data_size_from_storage_modules(entries);
        assert_eq!(result, expected);
    }

    proptest! {
        /// Property: publish ledger data_size is ALWAYS preferred over submit ledger,
        /// regardless of the relative sizes.
        #[test]
        fn publish_ledger_always_preferred_over_submit(
            publish_size in 1..u64::MAX,
            submit_size in 1..u64::MAX,
        ) {
            let result = select_data_size_from_storage_modules(vec![
                submit(submit_size),
                publish(publish_size),
            ]);

            let selected = result.expect("Should return Some when data exists");
            prop_assert_eq!(selected.data_size, publish_size);
            prop_assert!(selected.is_from_publish_ledger);
        }

        /// Property: single ledger type always selects max size
        #[test]
        fn single_ledger_type_selects_max(
            sizes in prop::collection::vec(1..u64::MAX, 1..10),
            is_publish in any::<bool>(),
        ) {
            let expected_max = *sizes.iter().max().unwrap();
            let entries: Vec<_> = sizes
                .into_iter()
                .map(|ds| DataSizeInfo {
                    data_size: ds,
                    is_from_publish_ledger: is_publish,
                })
                .collect();

            let result = select_data_size_from_storage_modules(entries);

            let selected = result.expect("Should return Some when data exists");
            prop_assert_eq!(selected.data_size, expected_max);
            prop_assert_eq!(selected.is_from_publish_ledger, is_publish);
        }

        /// Property: with mixed ledgers, publish ledger max is always selected
        #[test]
        fn mixed_ledgers_selects_publish_max(
            publish_sizes in prop::collection::vec(1..u64::MAX, 1..5),
            submit_sizes in prop::collection::vec(1..u64::MAX, 1..5),
        ) {
            let expected_publish_max = *publish_sizes.iter().max().unwrap();

            let mut entries: Vec<_> = publish_sizes.into_iter().map(publish).collect();
            entries.extend(submit_sizes.into_iter().map(submit));

            let result = select_data_size_from_storage_modules(entries);

            let selected = result.expect("Should return Some when data exists");
            prop_assert_eq!(selected.data_size, expected_publish_max);
            prop_assert!(selected.is_from_publish_ledger);
        }
    }
}
