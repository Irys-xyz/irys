use super::ChunkIngressServiceInner;
use super::ingress_proofs::generate_and_store_ingress_proof_from_leaves;
use super::metrics::{
    record_chunk_duplicate, record_chunk_error, record_chunk_ingested, record_enqueue_duration,
    record_flush_failure, record_validation_duration,
};
use super::{ChunkIngressMessage, ChunkIngressSource};

use irys_data_tree::{SizeError, VerifyError};
use irys_database::{complete_ingress_leaves, db::IrysDatabaseExt as _};
use irys_domain::{StorageModule, WriteDataChunkError};
use irys_types::gossip::v2::GossipBroadcastMessageV2;
use irys_types::{
    ChunkKey, DataCommitment, DatabaseProvider, H256, IngressMerkleLeaf, IngressProof,
    SendTraced as _, chunk::UnpackedChunk, data_tree::verify_data_chunk, irys::IrysSigner,
};
use irys_utils::ElapsedMs as _;
use lru::LruCache;
use reth::revm::primitives::alloy_primitives::ChainId;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{Instrument as _, debug, error, info, info_span, instrument, warn};

/// CDR-only ingress-proof eligibility gate, factored out so the contract is
/// unit-testable without standing up the full service.  Returns `true` iff
/// the caller should short-circuit `try_generate_ingress_proof_for_root`.
///
/// The commitment fixes the size, so one check remains: `block_set.is_empty()`,
/// a cheap "has this data_root been seen in any block yet?" gate.  `block_set`
/// is a hint with two writers (mempool `BlockConfirmed` pre-migration
/// forward-fill + `persist_metadata` canonical writer); a stale
/// `BlockConfirmed` processed after a reorg can leave an orphan hash here, so
/// non-empty does NOT prove canonical inclusion.  An over-permissive gate is
/// correctness-safe: ingress proofs are keyed on `data_commitment` (see the
/// `IngressProofs` table), so a proof generated for an orphan tx remains
/// valid if any future canonical tx has the same data_commitment — i.e.
/// speculative, not wasted.  Canonical truth is re-derived by
/// `tx_inclusion::find_canonical_ledger_range` at validation / promotion time.
/// An empty set, by contrast, means no writer has ever attached this data_root
/// to a block, so we short-circuit.  After a reorg that empties block_set, the
/// next canonical confirmation re-populates it via the mempool
/// `BlockConfirmed` path (`lifecycle.rs:96`).
fn should_skip_ingress_proof_generation(cdr: &irys_database::db_cache::CachedDataRoot) -> bool {
    cdr.block_set.is_empty()
}

/// Pure post-park rescue decision for the TX/chunk gossip TOCTOU.
///
/// After a chunk is parked, we re-read `CachedDataRoot`. This function decides
/// whether to schedule another `ProcessPendingChunks` drain:
/// - `cdr_found = Err(_)` → leave parked (do **not** fail the already-accepted park)
/// - `cdr_found = Ok(false)` → leave parked (still waiting for the header)
/// - `cdr_found = Ok(true)` → drain: the header that owns this commitment arrived
///
/// Extracted so deleting the rescue path without updating tests fails CI.
#[must_use]
pub(crate) fn should_schedule_pending_drain_after_park(cdr_found: Result<bool, ()>) -> bool {
    matches!(cdr_found, Ok(true))
}

/// Whether a popped pending chunk must be put back after a drain error.
///
/// `body_committed` is true only after the cache batch commits. Until then the
/// pending map is the only copy of the body. Validation rejects are permanent
/// and stay dropped. An error after the commit stays dropped: the cache row
/// holds the body.
#[must_use]
pub(crate) fn should_restore_pending_chunk_on_drain_error(
    err: &ChunkIngressError,
    body_committed: bool,
) -> bool {
    if body_committed {
        return false;
    }
    !is_permanent_chunk_reject(err)
}

fn is_permanent_chunk_reject(err: &ChunkIngressError) -> bool {
    matches!(
        err,
        ChunkIngressError::Critical(
            CriticalChunkIngressError::InvalidProof
                | CriticalChunkIngressError::InvalidChunkSize
                | CriticalChunkIngressError::InvalidDataSize
                | CriticalChunkIngressError::InvalidOffset(_)
        ) | ChunkIngressError::Advisory(
            AdvisoryChunkIngressError::PreHeaderOversizedBytes
                | AdvisoryChunkIngressError::PreHeaderOversizedDataPath
                | AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap
                | AdvisoryChunkIngressError::PreHeaderInvalidOffset(_)
        )
    )
}

/// What [`write_chunk_to_assigned_modules`] did with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModuleWriteOutcome {
    /// At least one module took the chunk.
    Written,
    /// A module had a writeable offset, but its writes are paused.
    Paused,
    /// No module had a writeable offset: no placement here, or the offset
    /// already holds data.
    NothingToWrite,
}

async fn write_chunk_to_assigned_modules(
    modules: &[Arc<StorageModule>],
    chunk: &UnpackedChunk,
) -> Result<ModuleWriteOutcome, ChunkIngressError> {
    let mut wrote_any = false;
    let mut paused = false;
    let mut in_flight = false;
    for sm in modules {
        let writeable_offsets = match sm.get_writeable_offsets(chunk) {
            Ok(offsets) => offsets,
            Err(error) => {
                error!(
                    ?error,
                    storage_module = sm.id,
                    data_root = ?chunk.data_tree_root,
                    tx_offset = %chunk.tx_offset,
                    "Failed to resolve writeable offsets"
                );
                return Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::DatabaseError,
                ));
            }
        };
        if writeable_offsets.is_empty() {
            if sm.has_in_flight_index_for(chunk) {
                in_flight = true;
            }
            continue;
        }
        info!(
            target: "irys::mempool::chunk_ingress",
            "Writing chunk with offset {} for data_root {} to sm {}",
            &chunk.tx_offset,
            &chunk.data_tree_root,
            &sm.id
        );
        match sm.write_data_chunk_queued(chunk).await {
            Ok(()) => wrote_any = true,
            Err(WriteDataChunkError::WritesPaused) => paused = true,
            Err(WriteDataChunkError::Backpressure) => {
                return Err(AdvisoryChunkIngressError::Overloaded.into());
            }
            Err(error) => {
                error!(
                    "Failed to write chunk data_root {:?} tx_offset {} to storage_module {}: {:?}",
                    chunk.data_tree_root, chunk.tx_offset, sm.id, error
                );
                return Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::Other(format!(
                        "Failed to write chunk to storage_module {}",
                        sm.id
                    )),
                ));
            }
        }
    }
    if !wrote_any && in_flight {
        // The bytes are already queued. The sender should retry later, not
        // treat this peer as having rejected the chunk.
        return Err(AdvisoryChunkIngressError::Overloaded.into());
    }
    Ok(if wrote_any {
        ModuleWriteOutcome::Written
    } else if paused {
        ModuleWriteOutcome::Paused
    } else {
        ModuleWriteOutcome::NothingToWrite
    })
}

async fn remember_after_module_writes(
    recent_valid_chunks: &RwLock<LruCache<ChunkKey, ()>>,
    recent_chunk: ChunkKey,
    write_result: Result<ModuleWriteOutcome, ChunkIngressError>,
) -> Result<(), ChunkIngressError> {
    if let Err(error) = write_result {
        // The ingress leaf is already written, so a key left over from an
        // earlier upload of the same data would make a retry skip this chunk.
        recent_valid_chunks.write().await.pop(&recent_chunk);
        return Err(error);
    }
    recent_valid_chunks.write().await.put(recent_chunk, ());
    Ok(())
}

impl ChunkIngressServiceInner {
    /// After parking a chunk, re-check `CachedDataRoot` and schedule
    /// `ProcessPendingChunks` if the concurrent TX committed in the park window.
    ///
    /// Never fails the already-accepted park: DB and channel errors warn only.
    /// Direct re-entry into `process_pending_chunks_for_root` would recurse
    /// through this async ingress stack (`E0733`), so we always go via the
    /// control-plane channel.
    fn maybe_rescue_after_park(&self, data_commitment: DataCommitment) {
        let cdr_found = self
            .irys_db
            .view_eyre(|read_tx| {
                irys_database::cached_data_root_by_commitment(read_tx, data_commitment)
            })
            .map(|cdr| cdr.is_some())
            .map_err(|e| {
                warn!(
                    chunk.data_commitment = %data_commitment,
                    error = ?e,
                    "post-park CDR recheck failed; chunk remains parked"
                );
            });

        if !should_schedule_pending_drain_after_park(cdr_found) {
            return;
        }

        debug!(
            chunk.data_commitment = %data_commitment,
            "CachedDataRoot usable after park; scheduling ProcessPendingChunks"
        );
        if let Err(e) = self
            .service_senders
            .chunk_ingress
            .send_traced(ChunkIngressMessage::ProcessPendingChunks(data_commitment))
        {
            warn!(
                chunk.data_commitment = %data_commitment,
                error = ?e,
                "Failed to schedule ProcessPendingChunks after post-park CDR recheck"
            );
        }
    }

    /// Limits on a chunk parked before its header arrives, so before its proof
    /// is checked: `bytes` at most `chunk_size`, `data_path` at most the
    /// configured max, and `tx_offset` valid for the claimed size.
    fn check_unconfirmed_chunk_bounds(
        &self,
        chunk: &UnpackedChunk,
        chunk_size: u64,
    ) -> Result<(), AdvisoryChunkIngressError> {
        let chunk_len_u64 = u64::try_from(chunk.bytes.len())
            .map_err(|_| AdvisoryChunkIngressError::PreHeaderOversizedBytes)?;
        if chunk_len_u64 > chunk_size {
            warn!(
                "Dropping pre-header chunk for {} at offset {}: bytes.len() {} exceeds chunk_size {}",
                &chunk.data_tree_root,
                &chunk.tx_offset,
                chunk.bytes.len(),
                chunk_size
            );
            return Err(AdvisoryChunkIngressError::PreHeaderOversizedBytes);
        }
        let preheader_data_path_max_bytes = self.config.mempool.max_preheader_data_path_bytes;
        if chunk.data_path.0.len() > preheader_data_path_max_bytes {
            warn!(
                "Dropping pre-header chunk for {} at offset {}: data_path too large ({} > {})",
                &chunk.data_tree_root,
                &chunk.tx_offset,
                chunk.data_path.0.len(),
                preheader_data_path_max_bytes
            );
            return Err(AdvisoryChunkIngressError::PreHeaderOversizedDataPath);
        }

        if !chunk.is_valid_offset(chunk_size) {
            let max_offset = chunk.max_valid_offset(chunk_size);
            warn!(
                "Dropping pre-header chunk for {} at offset {}: tx_offset {} exceeds max valid offset {:?} for data_size {}",
                &chunk.data_tree_root,
                &chunk.tx_offset,
                *chunk.tx_offset,
                max_offset,
                chunk.data_size
            );
            return Err(AdvisoryChunkIngressError::PreHeaderInvalidOffset(format!(
                "tx_offset {} exceeds max valid offset {:?} for data_size {}",
                *chunk.tx_offset, max_offset, chunk.data_size
            )));
        }
        Ok(())
    }

    /// Park `chunk` under one write lock.
    ///
    /// `Ok(true)` means the chunk parked while its root was draining. The caller
    /// must not start another drain: this drain schedules one follow-up when it
    /// ends. `Ok(false)` is a normal park. The cap error is a real full root,
    /// or this drain putting one of its own chunks back through the restore.
    async fn park_pending_chunk(
        &self,
        data_commitment: DataCommitment,
        chunk: Arc<UnpackedChunk>,
        cap: usize,
        role: PendingDrainRole,
        kind: &'static str,
    ) -> Result<bool, IngressFailure> {
        let data_root = chunk.data_tree_root;
        let tx_offset = chunk.tx_offset;
        let mut pending = self.pending_chunks.write().await;
        let current = pending
            .get(&data_commitment)
            .map(lru::LruCache::len)
            .unwrap_or(0);
        match pending.drain_admission(
            &data_commitment,
            current,
            cap,
            role == PendingDrainRole::Own,
        ) {
            super::pending_chunks::DrainAdmission::RejectOwn => {
                warn!(
                    "Rejecting {kind} chunk for {} at offset {}: data root drain in progress",
                    &data_root, &tx_offset
                );
                return Err(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap.into());
            }
            super::pending_chunks::DrainAdmission::RejectCap { occupied } => {
                warn!(
                    "Dropping {kind} chunk for {} at offset {}: cache full ({}/{})",
                    &data_root, &tx_offset, occupied, cap
                );
                return Err(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap.into());
            }
            super::pending_chunks::DrainAdmission::Accept => {
                pending.put(data_commitment, Arc::unwrap_or_clone(chunk));
                return Ok(true);
            }
            super::pending_chunks::DrainAdmission::Idle => {}
        }
        if current >= cap {
            warn!(
                "Dropping {kind} chunk for {} at offset {}: cache full ({}/{})",
                &data_root, &tx_offset, current, cap
            );
            return Err(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap.into());
        }
        pending.put(data_commitment, Arc::unwrap_or_clone(chunk));
        Ok(false)
    }

    /// Re-ingress chunks popped for one data root.
    ///
    /// A pre-commit error puts the chunk back and schedules another drain: the
    /// uploader will not resend a body that was already parked. A validation
    /// reject stays dropped. An error after the cache commit stays dropped. A
    /// first ingress that exceeds the cap stays rejected.
    ///
    /// This re-ingest does not park. Those chunks are put back here. An external
    /// chunk that arrives during the drain parks when the popped set plus that
    /// chunk still fit in the cap, and this function schedules one follow-up.
    /// A cap-only restore does not schedule itself: the next pass would
    /// cap-reject the same chunks. Those chunks wait for mempool postprocess
    /// or a post-park rescue. A cap restore does not grow past the popped set.
    /// A transient error may use `configured cap + popped len` slots, then
    /// stops. An offset that is already parked is replaced. Past the ceiling
    /// the chunk is logged and dropped. The drain flag is cleared under the
    /// same write lock as the restore.
    pub(super) async fn finish_pending_drain(
        &self,
        data_commitment: DataCommitment,
        chunks: Vec<UnpackedChunk>,
    ) {
        let popped_len = chunks.len();
        let mut saw_ok = false;
        let mut saw_retryable_restore = false;
        // `true` means the failure is the per-root cap (or a drain-in-progress reject).
        let mut restore: Vec<(Arc<UnpackedChunk>, bool)> = Vec::new();
        for chunk in chunks {
            let chunk = Arc::new(chunk);
            match self
                .ingest_chunk(
                    Arc::clone(&chunk),
                    PendingDrainRole::Own,
                    ChunkIngressSource::Fetched,
                )
                .await
            {
                Ok(()) => saw_ok = true,
                Err(failure) => {
                    record_chunk_error(failure.error.error_type(), failure.error.is_advisory());
                    error!(
                        "Failed to handle pending chunk ingress for data_commitment {:?}: {:?}",
                        data_commitment, failure.error
                    );
                    // The cap reject is permanent for a new ingress. This chunk
                    // was already accepted, then popped.
                    let cap_repark = !failure.body_committed
                        && matches!(
                            &failure.error,
                            ChunkIngressError::Advisory(
                                AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap
                            )
                        );
                    let retryable = should_restore_pending_chunk_on_drain_error(
                        &failure.error,
                        failure.body_committed,
                    );
                    if cap_repark || retryable {
                        restore.push((chunk, cap_repark));
                    }
                    if retryable {
                        saw_retryable_restore = true;
                    }
                }
            }
        }

        let configured_cap = std::cmp::min(
            self.config.node_config.mempool().max_chunks_per_item,
            self.config.mempool.max_preheader_chunks_per_item,
        );
        let mut kept_cap_restore = false;
        let arrived_during_drain;
        {
            let mut pending = self.pending_chunks.write().await;
            for (chunk, cap_limited) in restore {
                let chunk = Arc::try_unwrap(chunk).unwrap_or_else(|chunk| (*chunk).clone());
                let tx_offset = chunk.tx_offset;
                // Cap restores stay within the popped set. A transient error may
                // add that set on top of chunks that were already parked.
                let ceiling = if cap_limited {
                    configured_cap.max(popped_len)
                } else {
                    configured_cap.saturating_add(popped_len).max(popped_len)
                };
                if pending.get(&data_commitment).is_none() {
                    pending.put(data_commitment, chunk);
                    if cap_limited {
                        kept_cap_restore = true;
                    }
                    continue;
                }
                let parked = pending
                    .get_mut(&data_commitment)
                    .expect("pending root present for restore");
                let already_parked = parked.peek(&tx_offset).is_some();
                if already_parked {
                    parked.put(tx_offset, chunk);
                    if cap_limited {
                        kept_cap_restore = true;
                    }
                    continue;
                }
                if parked.len() >= ceiling {
                    warn!(
                        chunk.data_commitment = %data_commitment,
                        %tx_offset,
                        parked = parked.len(),
                        ceiling,
                        cap_limited,
                        "Dropped drained chunk restore: per-root pending ceiling is full"
                    );
                    continue;
                }
                // `LruCache::put` of a new key at capacity evicts a sibling.
                // Grow only up to the ceiling so an accepted offset is not evicted.
                if parked.len() >= parked.cap().get() {
                    let new_cap = std::num::NonZeroUsize::new(parked.len().saturating_add(1))
                        .expect("a non-empty cache grows by one slot");
                    parked.resize(new_cap);
                }
                parked.put(tx_offset, chunk);
                if cap_limited {
                    kept_cap_restore = true;
                }
            }
            arrived_during_drain = pending.take_arrived_and_clear_drain(&data_commitment);
        }

        // An Ok chunk that was only re-parked stays in the map. Scheduling on
        // that Ok spins. Schedule when a transient error remains, when some
        // chunk made progress and a cap-rejected sibling was put back, or when
        // an external chunk parked during this drain. A cap-only restore does
        // not schedule itself.
        let schedule =
            saw_retryable_restore || (saw_ok && kept_cap_restore) || arrived_during_drain;
        if !schedule {
            return;
        }
        debug!(
            chunk.data_commitment = %data_commitment,
            "Drained chunk put back; scheduling another ProcessPendingChunks"
        );
        schedule_pending_drain_retry(self.service_senders.clone(), data_commitment);
    }

    #[instrument(level = "info", skip_all, err(Debug), fields(chunk.data_root = ?chunk.data_tree_root, chunk.tx_offset = ?chunk.tx_offset))]
    pub(crate) async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
        source: ChunkIngressSource,
    ) -> Result<(), ChunkIngressError> {
        self.ingest_chunk(Arc::new(chunk), PendingDrainRole::External, source)
            .await
            .map_err(|failure| failure.error)
    }

    /// `source`: see [`ChunkIngressSource`].
    async fn ingest_chunk(
        &self,
        chunk: Arc<UnpackedChunk>,
        role: PendingDrainRole,
        source: ChunkIngressSource,
    ) -> Result<(), IngressFailure> {
        // TODO: maintain a shared read transaction so we have read isolation
        let chunk_size = self.config.consensus.chunk_size;

        info!(
            chunk.data_root = ?chunk.data_tree_root,
            chunk.tx_offset = %chunk.tx_offset,
            chunk.data_size = chunk.data_size,
            "Processing chunk"
        );

        // The chunk's key: its root and the size it claims. Only a header with
        // that exact (root, size) owns it, so a chunk never checks against
        // another size of the same root.
        let geometry = self.config.data_tree_geometry();
        let data_commitment = chunk.data_commitment(&geometry);

        let chunk_key = chunk.chunk_key(&geometry);

        // A row prune does not remove the key from `recent_valid_chunks`, so
        // a key can be left over from an earlier upload of the same data.
        // This node's ingress leaf for the chunk is deleted with the row, so
        // a present leaf proves that the chunk was processed under the
        // current row. Read the leaf only on a key hit: the address costs a
        // key derivation.
        let leaf_address = self
            .recent_valid_chunks
            .read()
            .await
            .contains(&chunk_key)
            .then(|| self.config.irys_signer().address());

        // Check to see if we have a cached data_root for this chunk. The leaf
        // is read in the same transaction, so it belongs to this row.
        let _fetch_size_span = info_span!("chunk.fetch_data_size").entered();
        let (cached_data_root, processed_under_current_row) = self
            .irys_db
            .view_eyre(|read_tx| {
                let cached_data_root =
                    irys_database::cached_data_root_by_commitment(read_tx, data_commitment)?;
                let has_ingress_leaf = match (leaf_address, &cached_data_root) {
                    (Some(address), Some(_)) => irys_database::cached_ingress_leaf(
                        read_tx,
                        data_commitment,
                        address,
                        chunk.tx_offset,
                    )?
                    .is_some(),
                    _ => false,
                };
                Ok((cached_data_root, has_ingress_leaf))
            })
            .map_err(|e| {
                error!(
                    "Database error fetching cached data_root for chunk data_root {:?} tx_offset {}: {:?}",
                    chunk.data_tree_root, chunk.tx_offset, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        drop(_fetch_size_span);

        // Early exit if we've already processed this chunk under the current
        // cache row: a recent key, the row and this node's leaf. A stale key
        // (its row was pruned) does not skip: a later upload of the same data
        // needs its body cached and its leaf written, whether its header
        // comes before its chunks or after.
        if processed_under_current_row {
            debug!(
                "Chunk {} already processed recently, skipping re-gossip",
                &chunk_key
            );
            record_chunk_duplicate();
            return Ok(());
        }

        let Some(cached_data_root) = cached_data_root else {
            // The cheap limits come before any index read.
            self.check_unconfirmed_chunk_bounds(chunk.as_ref(), chunk_size)?;

            // A sent chunk that this node already stored from its placement
            // and parked. A replay of it changes nothing, so it skips the
            // index reads and the proof check. The replay cache outlives the
            // parked chunk (a header drains it, or the pending map evicts it).
            // Only a chunk that is still parked is a replay: otherwise this is
            // new work for a later upload of the same data, which must park.
            if source == ChunkIngressSource::Sent
                && self.recent_placed_chunks.read().await.contains(&chunk_key)
                && self
                    .pending_chunks
                    .read()
                    .await
                    .get(&data_commitment)
                    .is_some_and(|parked| parked.peek(&chunk.tx_offset).is_some())
            {
                debug!(
                    "Chunk {} already stored from its placement recently, skipping",
                    &chunk_key
                );
                record_chunk_duplicate();
                return Ok(());
            }

            // No cache row: the row was pruned, or this node never cached the
            // header. A storage placement exists only for a tx that a block
            // included, so its commitment is anchored and its `data_size` is
            // the header's. Store the chunk in the modules that hold it.
            // A failed placement read parks the chunk, as before this lookup.
            match self.placement_data_size_by_commitment(data_commitment) {
                Ok(Some(data_size)) => {
                    return self
                        .ingest_placed_chunk(
                            chunk,
                            &geometry,
                            data_commitment,
                            data_size,
                            chunk_key,
                            role,
                            source,
                        )
                        .await;
                }
                Ok(None) => {}
                Err(error) => warn!(
                    ?error,
                    chunk.data_commitment = %data_commitment,
                    "Failed to read storage placements; parking the chunk"
                ),
            }

            // We don't have a data_root for this chunk but possibly the transaction containing this
            // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
            return self.park_until_header(data_commitment, chunk, role).await;
        };

        // The commitment binds the size, so the cached size is the size this
        // chunk claims.
        let chunk_len = verify_anchored_chunk(
            &geometry,
            data_commitment,
            cached_data_root.data_size,
            chunk.as_ref(),
        )?;

        // The background writer batches concurrent MDBX writes, but returns
        // only after the exact batch containing this chunk commits.
        let storage_start = Instant::now();
        let queued_write = match self
            .chunk_data_writer
            .queue_write(Arc::clone(&chunk))
            .instrument(info_span!("chunk.cache_write"))
            .await
        {
            Ok(queued) => queued,
            Err(e) => {
                error!(
                    "Failed to queue chunk cache write for data_root {:?} tx_offset {}: {:?}",
                    chunk.data_tree_root, chunk.tx_offset, e
                );
                return Err(CriticalChunkIngressError::DatabaseError.into());
            }
        };
        if queued_write.is_duplicate() {
            record_chunk_duplicate();
        }
        record_enqueue_duration(storage_start.elapsed_ms());

        // Commit the validated body and assignment-independent leaf before
        // making the chunk eligible for storage-module durability. Otherwise a
        // fast SM fsync could delete the cache row before this batch commits,
        // and a delayed writer could reinsert the body. The acknowledgement is
        // tied to this exact write, so another batch cannot mask its failure.
        if let Err(error) = queued_write.committed().await {
            error!(
                %error,
                data_root = %chunk.data_tree_root,
                tx_offset = %chunk.tx_offset,
                "Chunk cache batch failed"
            );
            record_flush_failure();
            return Err(CriticalChunkIngressError::DatabaseError.into());
        }

        record_chunk_ingested(chunk_len);

        // Write chunk to storage modules that have writeable offsets for this chunk.
        // Note: get_writeable_offsets() only returns offsets within the data_size
        // bounds for the data_root stored at that location in the storage module.
        // Clone Arcs and drop the modules guard before awaiting SM ACK so a
        // drain wait does not hold the std RwLock.
        let modules: Vec<_> = {
            let guard = self.storage_modules_guard.read();
            guard.iter().cloned().collect()
        };
        let write_result = write_chunk_to_assigned_modules(&modules, chunk.as_ref())
            .instrument(info_span!("chunk.write_storage_modules"))
            .await;
        remember_after_module_writes(&self.recent_valid_chunks, chunk_key, write_result)
            .await
            .map_err(|error| IngressFailure {
                error,
                body_committed: true,
            })?;

        // Gossip the chunk before moving onto ingress proof checks
        let chunk_data_root = chunk.data_tree_root;
        let chunk_tx_offset = chunk.tx_offset;
        let gossip_broadcast_message = GossipBroadcastMessageV2::chunk(chunk, &geometry);

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

        // Leaf persistence is assignment-independent: any staked node that
        // validates every chunk can produce the same proof it could before the
        // compact-leaf refactor. Storage-module durability separately authorizes
        // body reclamation; leaf presence alone is never an eviction fence.
        self.try_generate_ingress_proof_for_root(data_commitment, chunk_size)
            .map_err(|error| IngressFailure {
                error,
                body_committed: true,
            })?;

        // Storage-module writes retain their normal batching/striping policy.
        Ok(())
    }

    /// Park `chunk` under `data_commitment` until a header for it arrives,
    /// within the pre-header caps.
    async fn park_until_header(
        &self,
        data_commitment: DataCommitment,
        chunk: Arc<UnpackedChunk>,
        role: PendingDrainRole,
    ) -> Result<(), IngressFailure> {
        let preheader_chunks_per_item = std::cmp::min(
            self.config.node_config.mempool().max_chunks_per_item,
            self.config.mempool.max_preheader_chunks_per_item,
        );

        // The count check and the insert share one write lock so concurrent
        // ingress cannot exceed the per-item cap.
        let data_root = chunk.data_tree_root;
        let tx_offset = chunk.tx_offset;
        let during_drain = self
            .park_pending_chunk(
                data_commitment,
                chunk,
                preheader_chunks_per_item,
                role,
                "pre-header",
            )
            .await?;

        // A parked chunk is not cached. Log that, so Ok is not read as
        // a completed ingress.
        info!(
            chunk.data_root = %data_root,
            chunk.tx_offset = %tx_offset,
            "Parked chunk: no CachedDataRoot yet (awaiting tx header / CDR commit)"
        );

        // Close the TX/chunk gossip TOCTOU (see maybe_rescue_after_park).
        // Any CDR is enough: the header that commits it owns this root.
        // Skip this while a drain is active. That drain schedules one
        // follow-up; a second pop now would race its restore.
        if !during_drain {
            self.maybe_rescue_after_park(data_commitment);
        }
        Ok(())
    }

    /// The `data_size` of the first storage placement keyed by
    /// `data_commitment`. `None` when no module holds one. The key binds the
    /// size, so every placement under it has the same `data_size`.
    ///
    /// A module whose read fails is logged and skipped. The error is returned
    /// only when no module answered.
    fn placement_data_size_by_commitment(
        &self,
        data_commitment: DataCommitment,
    ) -> eyre::Result<Option<u64>> {
        let modules: Vec<_> = {
            let guard = self.storage_modules_guard.read();
            guard.iter().cloned().collect()
        };
        let mut any_answered = false;
        let mut last_error = None;
        for storage_module in modules {
            let read = storage_module.collect_data_root_infos(data_commitment);
            #[cfg(test)]
            let read = if tests::PLACEMENT_READ_FAULT_MODULES
                .with(|faulty| faulty.borrow().contains(&storage_module.id))
            {
                Err(eyre::eyre!("injected placement read error"))
            } else {
                read
            };
            match read {
                Ok(placements) => {
                    any_answered = true;
                    if let Some(placement) = placements.0.first() {
                        return Ok(Some(placement.data_size));
                    }
                }
                Err(error) => {
                    warn!(
                        ?error,
                        storage_module = storage_module.id,
                        chunk.data_commitment = %data_commitment,
                        "Failed to read storage placements from a module; trying the next module"
                    );
                    last_error = Some(error);
                }
            }
        }
        match last_error {
            Some(error) if !any_answered => Err(error),
            _ => Ok(None),
        }
    }

    /// Ingest a chunk that has a storage placement and no cache row.
    ///
    /// The chunk goes only to the modules that hold the placement. It does
    /// not write a cache row, a cached body or an ingress leaf, and it does
    /// not start ingress-proof generation: those belong to the cache row. It
    /// is not remembered in `recent_valid_chunks`, so the same chunk can still
    /// take the cached path when a header for this commitment arrives. It is
    /// not gossiped: a peer that holds the placement gets it by data sync.
    ///
    /// With [`ChunkIngressSource::Sent`] (a chunk that a client or a peer sent), the
    /// chunk also parks under its commitment after the module write. The
    /// same data can be uploaded again by a new tx with the same commitment,
    /// whose header arrives after its chunks. That header then drains the
    /// parked chunk through the cached path, so the body is cached and the
    /// ingress leaf is written without a resend. After the park the chunk key
    /// goes in `recent_placed_chunks`, so a replay is a no-op: the chunk is
    /// already stored and parked. The key is a replay only while the pending
    /// map still holds the chunk. Once a header drains it, or the pending map
    /// evicts it, a send of the same chunk takes the placed path again and
    /// parks again (a later upload of the same data needs its body and leaf).
    /// A failed park is not remembered, so a resend parks again.
    async fn ingest_placed_chunk(
        &self,
        chunk: Arc<UnpackedChunk>,
        geometry: &irys_types::data_tree::Geometry,
        data_commitment: DataCommitment,
        data_size: u64,
        chunk_key: ChunkKey,
        role: PendingDrainRole,
        source: ChunkIngressSource,
    ) -> Result<(), IngressFailure> {
        let chunk_len = verify_anchored_chunk(geometry, data_commitment, data_size, &chunk)?;

        // Clone Arcs and drop the modules guard before awaiting SM ACK.
        // `write_chunk_to_assigned_modules` writes only where this
        // commitment has a placement.
        let modules: Vec<_> = {
            let guard = self.storage_modules_guard.read();
            guard.iter().cloned().collect()
        };
        let outcome = write_chunk_to_assigned_modules(&modules, chunk.as_ref())
            .instrument(info_span!("chunk.write_storage_modules"))
            .await?;
        match outcome {
            ModuleWriteOutcome::Written => {
                record_chunk_ingested(chunk_len);
                info!(
                    chunk.data_commitment = %data_commitment,
                    chunk.tx_offset = %chunk.tx_offset,
                    "Stored chunk from its storage placement: no CachedDataRoot"
                );
            }
            // Nothing holds the bytes yet. The sender should send it again
            // later; this is not a fault of the sender.
            ModuleWriteOutcome::Paused => {
                return Err(AdvisoryChunkIngressError::Overloaded.into());
            }
            ModuleWriteOutcome::NothingToWrite => {
                debug!(
                    chunk.data_commitment = %data_commitment,
                    chunk.tx_offset = %chunk.tx_offset,
                    "Chunk from a storage placement is already stored"
                );
            }
        }
        if source == ChunkIngressSource::Fetched {
            return Ok(());
        }
        // The module write above is already done when the park runs. If the
        // park is rejected (per-item cap, or a drain in progress), the sender
        // gets an error although the chunk is stored. A resend verifies the
        // chunk again and repeats the module write, which is idempotent.
        self.park_until_header(data_commitment, chunk, role).await?;
        self.recent_placed_chunks.write().await.put(chunk_key, ());
        Ok(())
    }

    /// Checks whether an ingress proof should be generated for the given `data_commitment`
    /// and spawns proof generation if all prerequisites are met:
    /// - the CDR-only prerequisites in [`should_skip_ingress_proof_generation`]
    /// - no local proof already exists
    /// - every expected validated compact leaf is present with no gaps
    pub(crate) fn try_generate_ingress_proof_for_root(
        &self,
        data_commitment: DataCommitment,
        chunk_size: u64,
    ) -> Result<(), ChunkIngressError> {
        let cached_data_root = self
            .irys_db
            .view_eyre(|read_tx| {
                irys_database::cached_data_root_by_commitment(read_tx, data_commitment)
            })
            .map_err(|_| CriticalChunkIngressError::DatabaseError)?;

        // Early out: no cache row, no header for this commitment
        let Some(cdr) = cached_data_root else {
            return Ok(());
        };

        if should_skip_ingress_proof_generation(&cdr) {
            return Ok(());
        }

        // The commitment fixes the size, so the cached size is final
        let data_size = cdr.data_size;

        let local_address = self.config.irys_signer().address();
        let existing_local_proof = self
            .irys_db
            .view_eyre(|tx| {
                irys_database::ingress_proof_by_commitment_address(
                    tx,
                    data_commitment,
                    local_address,
                )
            })
            .map_err(|e| {
                error!(
                    "Database error checking ingress proof for data_commitment {:?}: {:?}",
                    data_commitment, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        // Early return if we have a valid existing local proof
        if existing_local_proof.is_some() {
            self.ingress_proof_generation_state
                .clear_retries(data_commitment);
            info!(
                "Local ingress proof already exists and is valid for data root {}",
                &data_commitment
            );
            return Ok(());
        }

        // `complete_ingress_leaves` uses a dup-count only as a cheap prefilter,
        // then verifies every exact offset and byte boundary before returning.
        let leaves = self
            .irys_db
            .view_eyre(|tx| {
                complete_ingress_leaves(tx, data_commitment, local_address, data_size, chunk_size)
            })
            .map_err(|error| {
                error!(
                    ?data_commitment,
                    ?error,
                    "Database error loading compact ingress leaves"
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        if let Some(leaves) = leaves {
            // Acquire only after this snapshot is complete. If an incomplete
            // scan held the lease while the final writer committed, the final
            // handler could lose its only proof-generation wake-up to
            // contention. Every lease holder is therefore ready to make
            // progress, and competing complete scans can safely coalesce.
            let Some(generation_lease) = self
                .ingress_proof_generation_state
                .try_acquire(data_commitment)
            else {
                return Ok(());
            };
            let db = self.irys_db.clone();
            let block_tree_read_guard = self.block_tree_read_guard.clone();
            let config = self.config.clone();
            let gossip_sender = self.service_senders.gossip_broadcast.clone();
            let retry_sender = self.service_senders.chunk_ingress.clone();
            let generation_state = self.ingress_proof_generation_state.clone();
            let runtime_handle = tokio::runtime::Handle::current();
            let _fut = self.exec.clone().spawn_blocking(move || {
                match generate_and_store_ingress_proof_from_leaves(
                    &block_tree_read_guard,
                    &db,
                    &config,
                    data_commitment,
                    None,
                    leaves,
                    &gossip_sender,
                    generation_lease,
                ) {
                    Ok(_) => generation_state.clear_retries(data_commitment),
                    Err(error) if error.is_benign() => {
                        debug!(proof.data_commitment = ?data_commitment, "Skipped ingress proof generation: {error}");
                    }
                    Err(error) => {
                        warn!(proof.data_commitment = ?data_commitment, "Failed to generate ingress proof: {error}");
                        if let Some(delay) = generation_state.reserve_retry(data_commitment) {
                            let retry_state = generation_state.clone();
                            runtime_handle.spawn(async move {
                                tokio::time::sleep(delay).await;
                                retry_state.mark_retry_dispatched(data_commitment);
                                if let Err(send_error) = retry_sender.send_traced(
                                    ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(vec![
                                        data_commitment,
                                    ]),
                                ) {
                                    retry_state.clear_retries(data_commitment);
                                    warn!(
                                        proof.data_commitment = ?data_commitment,
                                        ?send_error,
                                        "Failed to schedule ingress-proof retry"
                                    );
                                }
                            });
                        }
                    }
                }
            }).in_current_span();
        }
        Ok(())
    }
}

/// Ingress failure, and whether the chunk body is already in the cache.
struct IngressFailure {
    error: ChunkIngressError,
    /// True only after `queued_write.committed()` succeeds.
    body_committed: bool,
}

impl From<ChunkIngressError> for IngressFailure {
    fn from(error: ChunkIngressError) -> Self {
        Self {
            error,
            body_committed: false,
        }
    }
}

impl From<CriticalChunkIngressError> for IngressFailure {
    fn from(error: CriticalChunkIngressError) -> Self {
        ChunkIngressError::from(error).into()
    }
}

impl From<AdvisoryChunkIngressError> for IngressFailure {
    fn from(error: AdvisoryChunkIngressError) -> Self {
        ChunkIngressError::from(error).into()
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
            Self::InvalidChunkSize => "invalid_chunk_size",
            Self::InvalidDataSize => "invalid_data_size",
            Self::InvalidOffset(_) => "invalid_offset",
            Self::DatabaseError => "database_error",
            Self::ServiceUninitialized => "service_uninitialized",
            Self::Other(_) => "other",
        }
    }
}

/// Check `chunk` against an anchored `data_commitment` and the `data_size`
/// it binds. Returns the chunk length. Every error is the sender's fault.
fn verify_anchored_chunk(
    geometry: &irys_types::data_tree::Geometry,
    data_commitment: DataCommitment,
    data_size: u64,
    chunk: &UnpackedChunk,
) -> Result<u64, CriticalChunkIngressError> {
    // Validate the data_path/proof for the chunk, linking
    // data_root->chunk_hash
    let _proof_span = info_span!("chunk.validate_proof").entered();

    if data_size == 0 {
        error!(
            "Error: {:?}. Invalid data_size for data_root: {:?}. got 0 bytes",
            CriticalChunkIngressError::InvalidDataSize,
            chunk.data_tree_root,
        );
        return Err(CriticalChunkIngressError::InvalidDataSize);
    }

    let validation_start = Instant::now();
    let chunk_len = u64::try_from(chunk.bytes.len())
        .map_err(|_| CriticalChunkIngressError::InvalidChunkSize)?;
    if let Err(error) = verify_data_chunk(
        geometry,
        data_commitment,
        data_size,
        chunk.tx_offset,
        &chunk.bytes.0,
        &chunk.data_path.0,
    ) {
        warn!(
            %error,
            chunk.tx_offset = %chunk.tx_offset,
            chunk_len,
            "Chunk failed verification"
        );
        return Err(ingest_error_for_verify_failure(error));
    }

    record_validation_duration(validation_start.elapsed_ms());
    Ok(chunk_len)
}

/// Ingest error for a chunk that fails [`verify_data_chunk`]. Each result is
/// a permanent reject of invalid peer data.
fn ingest_error_for_verify_failure(error: VerifyError) -> CriticalChunkIngressError {
    match error {
        VerifyError::Size(SizeError::DataSizeOutOfRange { .. }) => {
            CriticalChunkIngressError::InvalidDataSize
        }
        VerifyError::Size(error) => CriticalChunkIngressError::InvalidOffset(error.to_string()),
        VerifyError::InvalidChunkLength { .. } => CriticalChunkIngressError::InvalidChunkSize,
        _ => CriticalChunkIngressError::InvalidProof,
    }
}

#[derive(Debug, Clone)]
pub enum AdvisoryChunkIngressError {
    PreHeaderOversizedBytes,
    PreHeaderOversizedDataPath,
    PreHeaderOffsetExceedsCap,
    PreHeaderInvalidOffset(String),
    /// Service is at capacity and cannot accept the chunk right now. Caller
    /// should retry later. Distinct from a network failure: the peer is fine,
    /// the receiver is just saturated.
    Overloaded,
    Other(String),
}

impl AdvisoryChunkIngressError {
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::PreHeaderOversizedBytes => "pre_header_oversized_bytes",
            Self::PreHeaderOversizedDataPath => "pre_header_oversized_data_path",
            Self::PreHeaderOffsetExceedsCap => "pre_header_offset_exceeds_cap",
            Self::PreHeaderInvalidOffset(_) => "pre_header_invalid_offset",
            Self::Overloaded => "overloaded",
            Self::Other(_) => "other",
        }
    }
}

/// Generates an ingress proof for a specific `data_commitment` from its ordered,
/// persisted compact leaves. Cached chunk bodies are not read on this path.
#[must_use = "the generated ingress proof should be used or stored"]
pub fn generate_ingress_proof(
    db: DatabaseProvider,
    data_commitment: DataCommitment,
    leaves: Vec<IngressMerkleLeaf>,
    signer: IrysSigner,
    chain_id: ChainId,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let proof = irys_types::ingress::generate_ingress_proof_from_leaves(
        &signer,
        data_commitment,
        leaves,
        chain_id,
        anchor,
    )?;

    info!(
        "generated ingress proof {} for data commitment {}",
        &proof.proof, &data_commitment
    );
    db.update_scoped(|rw_tx| irys_database::store_ingress_proof_checked(rw_tx, &proof, &signer))??;

    Ok(proof)
}

/// Wait before redraining a data root after a pre-commit error or a chunk
/// that arrived while the root was draining. One delay keeps a persistent
/// miss from spinning the control plane.
const PENDING_DRAIN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Schedule one `ProcessPendingChunks` after `PENDING_DRAIN_RETRY_DELAY`.
pub(super) fn schedule_pending_drain_retry(
    senders: crate::services::ServiceSenders,
    data_commitment: DataCommitment,
) {
    tokio::spawn(async move {
        tokio::time::sleep(PENDING_DRAIN_RETRY_DELAY).await;
        if let Err(error) = senders
            .chunk_ingress
            .send_traced(ChunkIngressMessage::ProcessPendingChunks(data_commitment))
        {
            warn!(
                chunk.data_commitment = %data_commitment,
                ?error,
                "Failed to schedule ProcessPendingChunks after putting a drained chunk back"
            );
        }
    });
}

/// External ingress may park during a drain. The drain's own re-ingest must not:
/// the restore puts those chunks back without evicting a sibling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingDrainRole {
    External,
    Own,
}

#[cfg(test)]
mod tests {
    thread_local! {
        /// Storage module ids whose placement read on the no-row path fails.
        pub(super) static PLACEMENT_READ_FAULT_MODULES: std::cell::RefCell<Vec<usize>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Parks a chunk under its own commitment, the way ingress does.
    trait ParkUnderCommitment {
        fn park(&mut self, chunk: UnpackedChunk);
    }

    impl ParkUnderCommitment for super::super::pending_chunks::PriorityPendingChunks {
        fn park(&mut self, chunk: UnpackedChunk) {
            let data_commitment = chunk.data_commitment(&irys_types::data_tree::Geometry::TESTING);
            self.put(data_commitment, chunk);
        }
    }

    use super::*;
    use irys_database::db_cache::CachedDataRoot;
    use irys_types::DataTreeRoot;
    use irys_types::TxChunkOffset;
    use irys_types::UnixTimestamp;

    /// Build a `CachedDataRoot` with explicit gate-relevant fields.
    fn cdr_gate_fixture(
        block_set: Vec<H256>,
        txid_set: Vec<H256>,
        expiry_height: Option<u64>,
    ) -> CachedDataRoot {
        CachedDataRoot {
            data_size: 64,
            txid_set,
            block_set,
            expiry_height,
            cached_at: UnixTimestamp::from_secs(0),
            data_tree_root: Default::default(),
        }
    }

    /// Happy path: any block in `block_set` → proceed.
    #[test]
    fn ingress_gate_proceeds_when_block_set_populated() {
        let cdr = cdr_gate_fixture(vec![H256::random()], vec![H256::random()], None);
        assert!(!should_skip_ingress_proof_generation(&cdr));
    }

    /// A database error before the cache commit is the only copy of the body.
    #[test]
    fn precommit_database_error_restores_pending_chunk() {
        let err = ChunkIngressError::Critical(CriticalChunkIngressError::DatabaseError);
        assert!(should_restore_pending_chunk_on_drain_error(&err, false));
        assert!(!should_restore_pending_chunk_on_drain_error(&err, true));
        let overloaded = ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded);
        assert!(should_restore_pending_chunk_on_drain_error(
            &overloaded,
            false
        ));
    }

    /// Validation rejects stay dropped. So does any error after the cache commit.
    #[test]
    fn validation_and_post_commit_errors_do_not_restore_pending_chunk() {
        let validation = [
            ChunkIngressError::Critical(CriticalChunkIngressError::InvalidProof),
            ChunkIngressError::Critical(CriticalChunkIngressError::InvalidChunkSize),
            ChunkIngressError::Critical(CriticalChunkIngressError::InvalidDataSize),
            ChunkIngressError::Critical(CriticalChunkIngressError::InvalidOffset("offset".into())),
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedBytes),
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedDataPath),
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap),
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderInvalidOffset(
                "offset".into(),
            )),
        ];
        for err in &validation {
            assert!(
                !should_restore_pending_chunk_on_drain_error(err, false),
                "{err:?}"
            );
        }
        let overloaded = ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded);
        assert!(!should_restore_pending_chunk_on_drain_error(
            &overloaded,
            true
        ));
        let other = ChunkIngressError::Critical(CriticalChunkIngressError::Other("sm".into()));
        assert!(!should_restore_pending_chunk_on_drain_error(&other, true));
    }

    /// Every chunk-verification failure stays a permanent reject of peer data.
    #[test]
    fn every_verify_failure_is_a_permanent_reject() {
        let failures = [
            VerifyError::Size(SizeError::DataSizeOutOfRange {
                data_size: 0,
                max_data_size: 1,
            }),
            VerifyError::Size(SizeError::ChunkIndexOutOfRange {
                chunk_index: 2,
                chunk_count: 2,
            }),
            VerifyError::InvalidChunkLength {
                expected: 32,
                actual: 31,
            },
            VerifyError::InvalidProofLength {
                expected: 64,
                actual: 63,
            },
            VerifyError::SizeWitnessMismatch,
            VerifyError::CommitmentMismatch,
        ];
        for failure in failures {
            let error = ChunkIngressError::Critical(ingest_error_for_verify_failure(failure));
            assert!(
                is_permanent_chunk_reject(&error),
                "{failure:?} -> {error:?}"
            );
            assert!(
                crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
                "{failure:?} -> {error:?}"
            );
        }
    }

    /// Pre-header park (no CDR at first look): any post-park CDR should drain.
    #[test]
    fn post_park_rescue_schedules_when_any_cdr_appears() {
        assert!(should_schedule_pending_drain_after_park(Ok(true)));
        assert!(!should_schedule_pending_drain_after_park(Ok(false)));
    }

    /// DB error after park must leave the chunk parked — not fail the ingest
    /// and not skip rescue via `?` (which would reintroduce the strand).
    #[test]
    fn post_park_rescue_leaves_parked_on_db_error() {
        assert!(!should_schedule_pending_drain_after_park(Err(())));
    }

    /// Not-in-block-set state: pending tx with chunks staged but
    /// no `BlockConfirmed` has populated `block_set` yet.  Today the gate
    /// **short-circuits** — pinning this so a future widening to a
    /// `txid_set` + canonical-metadata fall-through is a deliberate change.
    #[test]
    fn ingress_gate_skips_when_txid_set_populated_but_block_set_empty() {
        let cdr = cdr_gate_fixture(vec![], vec![H256::random()], Some(100));
        assert!(
            should_skip_ingress_proof_generation(&cdr),
            "current contract: empty block_set short-circuits even when txid_set is populated"
        );
    }

    #[tokio::test]
    async fn failed_sm_write_does_not_remember_chunk_for_retry() -> eyre::Result<()> {
        use irys_domain::{StorageModule, StorageModuleInfo};
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::{
            Config, ConsensusConfig, DataTransactionHeader, DataTransactionHeaderV1,
            DataTransactionMetadata, LedgerChunkOffset, LedgerChunkRange, NodeConfig,
            PartitionChunkOffset, TxChunkOffset, ledger_chunk_offset_ii,
            partition::PartitionAssignment, partition_chunk_offset_ii,
        };
        use nodit::interval::ii;
        use std::num::NonZeroUsize;

        let tmp = TempDirBuilder::new()
            .prefix("ingress_sm_fail_no_lru")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                // Four segments do not divide five bytes.
                segments_per_chunk: 1,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = Arc::new(StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(irys_types::DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?);
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_tree_root: DataTreeRoot(data_root),
                data_size: 5,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        let chunk_key = chunk.chunk_key(&irys_types::data_tree::Geometry::TESTING);
        let recent = RwLock::new(LruCache::new(NonZeroUsize::new(8).expect("nonzero")));
        // A key left over from an earlier upload of the same data.
        recent.write().await.put(chunk_key, ());

        storage_module.fail_next_index_commit();
        let first = write_chunk_to_assigned_modules(&[Arc::clone(&storage_module)], &chunk).await;
        remember_after_module_writes(&recent, chunk_key, first)
            .await
            .expect_err("SM Other must not remember the hash");
        assert!(
            !recent.read().await.contains(&chunk_key),
            "retry must not hit recent_valid_chunks after a failed SM write"
        );

        remember_after_module_writes(
            &recent,
            chunk_key,
            write_chunk_to_assigned_modules(&[storage_module], &chunk).await,
        )
        .await?;
        assert!(recent.read().await.contains(&chunk_key));
        Ok(())
    }

    /// A chunk's key is its own root and size. With only a header of another
    /// size cached, the chunk parks under its own commitment and is checked
    /// against no other size. The header with its size drains it, and it then
    /// verifies and lands in the cache under that commitment only.
    #[tokio::test]
    async fn chunk_parks_under_its_own_commitment_until_that_header_arrives() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-own-commitment")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );

        // Three chunks of 32 bytes under one tree root.
        let data: Vec<u8> = (0..96_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        let last_chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 96,
            data_path: proofs[2].clone().into(),
            bytes: data[64..96].to_vec().into(),
            tx_offset: TxChunkOffset::from(2),
        };
        let short_key = size_key(data_root, 64);
        let long_key = size_key(data_root, 96);

        // Header A: same root, 64 bytes.
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner
            .handle_chunk_ingress_message(last_chunk.clone(), ChunkIngressSource::Sent)
            .await
            .expect("a chunk whose size no header names parks; it is not a fault");
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&long_key)
                .expect("the chunk parks under its own commitment");
            assert!(parked.peek(&TxChunkOffset::from(2)).is_some());
            assert!(
                pending.get(&short_key).is_none(),
                "the 64-byte header owns no part of this chunk"
            );
        }

        // Header B: same root, 96 bytes. Its arrival drains its own commitment.
        insert_cached_data_root(&inner.irys_db, data_root, 96);
        inner.process_pending_chunks_for_root(long_key).await;

        assert!(
            inner.pending_chunks.read().await.get(&long_key).is_none(),
            "the drain leaves nothing parked"
        );
        inner.irys_db.view_eyre(|tx| {
            let (_, cached) = cached_chunk_by_chunk_offset(tx, long_key, TxChunkOffset::from(2))?
                .expect("the verified chunk is cached under its commitment");
            assert_eq!(cached.chunk, Some(last_chunk.bytes.clone()));
            assert!(
                cached_chunk_by_chunk_offset(tx, short_key, TxChunkOffset::from(2))?.is_none(),
                "the chunk is not cached under the other size"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// The v2 tree of `data` under the testing geometry: the root and one
    /// proof per chunk.
    fn data_tree_with_proofs(data: &[u8]) -> eyre::Result<(H256, Vec<Vec<u8>>)> {
        let tree = irys_data_tree::DataTree::from_bytes(irys_data_tree::Geometry::TESTING, data)?;
        let proofs = (0..tree.chunk_count())
            .map(|chunk_index| tree.chunk_proof(chunk_index))
            .collect::<Result<_, _>>()?;
        Ok((H256(*tree.data_tree_root().as_bytes()), proofs))
    }

    /// A known header fixes the length of each chunk. A body that is not
    /// `chunk_len(data_size, chunk_index)` bytes is a peer fault, not a silent drop.
    #[tokio::test]
    async fn wrong_length_chunk_is_a_peer_fault() -> eyre::Result<()> {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-wrong-length")
            .build();
        let (inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        // 40 bytes at `chunk_size` 32: chunk 0 holds 32 bytes, chunk 1 holds 8.
        let data: Vec<u8> = (0..40_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        insert_cached_data_root(&inner.irys_db, data_root, 40);
        let mut nine_bytes = data[32..40].to_vec();
        nine_bytes.push(0);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 40,
            data_path: proofs[1].clone().into(),
            bytes: nine_bytes.into(),
            tx_offset: TxChunkOffset::from(1),
        };

        let error = inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await
            .expect_err("a chunk of the wrong length must be rejected");
        assert!(
            matches!(
                error,
                ChunkIngressError::Critical(CriticalChunkIngressError::InvalidChunkSize)
            ),
            "{error:?}"
        );
        assert!(
            crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
            "a wrong chunk length is the sender's fault"
        );
        Ok(())
    }

    /// Two headers share one tree root with sizes 64 and 96, but the root is
    /// the 96-byte tree's. The 96-byte upload completes. The 64-byte upload is
    /// a chunk-aligned prefix of that tree: under v2 the root binds the tree
    /// shape, so its chunks fail against the 64-byte commitment and each one
    /// is a peer fault.
    #[tokio::test]
    async fn same_root_two_sizes_only_the_built_size_completes() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-same-root-two-sizes")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        // Three chunks of 32 bytes. The 64-byte prefix is the first two.
        let data: Vec<u8> = (0..96_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        insert_cached_data_root(&inner.irys_db, data_root, 96);
        let chunk_of = |data_size: u64, chunk_index: u32| {
            let start = chunk_index as usize * 32;
            UnpackedChunk {
                data_tree_root: DataTreeRoot(data_root),
                data_size,
                data_path: proofs[chunk_index as usize].clone().into(),
                bytes: data[start..start + 32].to_vec().into(),
                tx_offset: TxChunkOffset::from(chunk_index),
            }
        };

        for (data_size, chunk_index) in [(96, 2), (64, 1), (96, 0), (64, 0), (96, 1)] {
            let result = inner
                .handle_chunk_ingress_message(
                    chunk_of(data_size, chunk_index),
                    ChunkIngressSource::Sent,
                )
                .await;
            if data_size == 96 {
                result.unwrap_or_else(|error| {
                    panic!("chunk {chunk_index} of the 96-byte upload: {error:?}")
                });
            } else {
                let error = result.expect_err("a prefix-size chunk must not verify");
                assert!(
                    matches!(
                        error,
                        ChunkIngressError::Critical(CriticalChunkIngressError::InvalidProof)
                    ),
                    "chunk {chunk_index} of the 64-byte upload: {error:?}"
                );
                assert!(
                    crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
                    "a prefix-size chunk is the sender's fault"
                );
            }
        }

        inner.irys_db.view_eyre(|tx| {
            for chunk_index in 0..3_u32 {
                let (_, cached) = cached_chunk_by_chunk_offset(
                    tx,
                    size_key(data_root, 96),
                    TxChunkOffset::from(chunk_index),
                )?
                .unwrap_or_else(|| panic!("chunk {chunk_index} of the 96-byte upload is cached"));
                assert_eq!(cached.chunk, Some(chunk_of(96, chunk_index).bytes));
            }
            for chunk_index in 0..2_u32 {
                assert!(
                    cached_chunk_by_chunk_offset(
                        tx,
                        size_key(data_root, 64),
                        TxChunkOffset::from(chunk_index),
                    )?
                    .is_none(),
                    "chunk {chunk_index} of the 64-byte upload is not cached"
                );
            }
            Ok(())
        })?;
        Ok(())
    }

    /// A header that names a chunk-aligned prefix size of a larger v2 tree
    /// never completes. Under v2 the root binds the tree shape. The honest
    /// proofs of the 96-byte tree carry its size witness, so none verifies
    /// under the 64-byte header. A forged proof can open chunk 1 (data tree
    /// spec §6): chunk 2's proof with the 64-byte size witness climbs from
    /// chunk 2's bytes to the root. Chunk 0 can never verify: it would need
    /// a chunk root equal to `node(c0, c1)`, which the domain tags forbid.
    /// So the ingress leaves stay incomplete and no ingress proof is made.
    #[tokio::test]
    async fn prefix_header_of_a_larger_tree_never_completes() -> eyre::Result<()> {
        use irys_database::db::IrysDatabaseExt as _;

        const PREFIX_DATA_SIZE: u64 = 64;
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-prefix-header")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let data: Vec<u8> = (0..96_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        insert_cached_data_root(&inner.irys_db, data_root, PREFIX_DATA_SIZE);

        for (chunk_index, proof) in proofs.iter().enumerate() {
            let start = chunk_index * 32;
            let chunk = UnpackedChunk {
                data_tree_root: DataTreeRoot(data_root),
                data_size: PREFIX_DATA_SIZE,
                data_path: proof.clone().into(),
                bytes: data[start..start + 32].to_vec().into(),
                tx_offset: TxChunkOffset::from(u32::try_from(chunk_index)?),
            };
            let error = inner
                .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
                .await
                .expect_err("no honest proof of the larger tree verifies under the prefix header");
            assert!(
                crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
                "chunk {chunk_index}: {error:?}"
            );
        }

        // Chunk 2's proof is `[node(c0, c1), size_witness(96)]`. With the
        // 64-byte witness it opens chunk 2's bytes as chunk 1 of 64 bytes.
        let mut forged_proof = proofs[2].clone();
        let witness_start = forged_proof.len() - irys_data_tree::HASH_SIZE;
        forged_proof[witness_start..].copy_from_slice(
            irys_data_tree::size_witness(&irys_data_tree::Geometry::TESTING, PREFIX_DATA_SIZE)
                .as_bytes(),
        );
        inner
            .handle_chunk_ingress_message(
                UnpackedChunk {
                    data_tree_root: DataTreeRoot(data_root),
                    data_size: PREFIX_DATA_SIZE,
                    data_path: forged_proof.into(),
                    bytes: data[64..96].to_vec().into(),
                    tx_offset: TxChunkOffset::from(1),
                },
                ChunkIngressSource::Sent,
            )
            .await
            .expect("the forged proof opens chunk 1 of the prefix size");

        let commitment = size_key(data_root, PREFIX_DATA_SIZE);
        let address = inner.config.irys_signer().address();
        inner.irys_db.view_eyre(|tx| {
            assert!(
                complete_ingress_leaves(tx, commitment, address, PREFIX_DATA_SIZE, 32)?.is_none(),
                "the prefix header has no complete ingress-leaf set"
            );
            assert!(
                irys_database::ingress_proof_by_commitment_address(tx, commitment, address)?
                    .is_none(),
                "the prefix header has no ingress proof"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// A chunk whose `data_size` no header names parks under its own
    /// commitment. It is not checked against the known size of the same root
    /// and is not a fault. The pending-entry cap evicts it like any other
    /// chunk that waits for its header.
    #[tokio::test]
    async fn chunk_with_an_unknown_size_parks_and_is_not_a_fault() -> eyre::Result<()> {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-unknown-size")
            .build();
        let (inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let data: Vec<u8> = (0..96_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        let unknown_size_chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 96,
            data_path: proofs[2].clone().into(),
            bytes: data[64..96].to_vec().into(),
            tx_offset: TxChunkOffset::from(2),
        };

        inner
            .handle_chunk_ingress_message(unknown_size_chunk, ChunkIngressSource::Sent)
            .await
            .expect("a chunk whose size no header names parks; it is not a fault");
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&size_key(data_root, 96))
                .expect("the chunk parks under its own commitment");
            assert!(parked.peek(&TxChunkOffset::from(2)).is_some());
            assert!(
                pending.get(&size_key(data_root, 64)).is_none(),
                "the 64-byte header owns no part of this chunk"
            );
        }

        // Other uploads that wait for their headers fill the pending-entry
        // cap. Each holds two chunks, so the one-chunk entry is evicted first.
        for other_upload in 1..=PENDING_ENTRY_CAP as u8 {
            let other_root = H256([other_upload; 32]);
            for chunk_index in 0..2_u32 {
                inner
                    .handle_chunk_ingress_message(
                        chunk_with(other_root, chunk_index, other_upload, CLAIMED_DATA_SIZE),
                        ChunkIngressSource::Sent,
                    )
                    .await
                    .expect("a chunk that waits for its header parks");
            }
        }
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&size_key(data_root, 96))
                .is_none(),
            "the full pending-entry cap evicts the parked chunk"
        );
        Ok(())
    }

    /// One storage module that holds a placement for a two-chunk upload of
    /// 64 bytes at ledger chunks 0..=1, and no cache row for it. Returns the
    /// module, the data and the chunk proofs.
    fn place_upload_without_cache_row(
        inner: &mut ChunkIngressServiceInner,
    ) -> eyre::Result<(Arc<StorageModule>, Vec<u8>, H256, Vec<Vec<u8>>)> {
        use irys_domain::{StorageModuleInfo, StorageModulesReadGuard};
        use irys_types::{
            DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata, LedgerChunkOffset, LedgerChunkRange, PartitionChunkOffset,
            ledger_chunk_offset_ii, partition::PartitionAssignment, partition_chunk_offset_ii,
        };
        use nodit::interval::ii;

        let data: Vec<u8> = (0..64_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        let storage_module = Arc::new(StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(irys_types::DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &inner.config,
        )?);
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_tx = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_tree_root: DataTreeRoot(data_root),
                data_size: 64,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 1)),
            0,
        )?;
        inner.storage_modules_guard = StorageModulesReadGuard::new(Arc::new(
            std::sync::RwLock::new(vec![Arc::clone(&storage_module)]),
        ));
        Ok((storage_module, data, data_root, proofs))
    }

    /// Ingress leaves stored for `data_commitment` and `address`, at any offset.
    fn ingress_leaf_count(
        tx: &impl irys_database::reth_db::transaction::DbTx,
        data_commitment: DataCommitment,
        address: irys_types::IrysAddress,
    ) -> eyre::Result<usize> {
        use irys_database::reth_db::cursor::DbDupCursorRO as _;

        let mut cursor = tx.cursor_dup_read::<irys_database::tables::CachedIngressLeaves>()?;
        let key = irys_database::db_cache::CachedIngressLeafKey {
            data_commitment,
            address,
        };
        Ok(cursor.walk_dup(Some(key), None)?.count())
    }

    /// Delete what `cache_service` deletes when it prunes the cache row of
    /// `data_commitment`: its ingress proofs, cached chunks, ingress leaves
    /// and the row. `recent_valid_chunks` is not changed.
    fn prune_cache_row(
        inner: &ChunkIngressServiceInner,
        data_commitment: DataCommitment,
    ) -> eyre::Result<()> {
        use irys_database::reth_db::transaction::DbTxMut as _;
        use irys_database::{
            db::IrysDatabaseExt as _,
            delete_cached_chunks_by_data_root, delete_ingress_leaves_by_data_root,
            tables::{CachedDataRoots, IngressProofs},
        };

        inner.irys_db.update_eyre(|tx| {
            tx.delete::<IngressProofs>(data_commitment, None)?;
            delete_cached_chunks_by_data_root(tx, data_commitment)?;
            delete_ingress_leaves_by_data_root(tx, data_commitment)?;
            tx.delete::<CachedDataRoots>(data_commitment, None)?;
            Ok(())
        })
    }

    /// Nothing for `data_commitment` is in the chunk cache: no cache row, no
    /// cached body, no ingress leaf and no ingress proof.
    fn assert_no_cache_state(inner: &ChunkIngressServiceInner, data_commitment: DataCommitment) {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let address = inner.config.irys_signer().address();
        inner
            .irys_db
            .view_eyre(|tx| {
                assert!(
                    irys_database::cached_data_root_by_commitment(tx, data_commitment)?.is_none(),
                    "no cache row is written"
                );
                for chunk_index in 0..2_u32 {
                    assert!(
                        cached_chunk_by_chunk_offset(
                            tx,
                            data_commitment,
                            TxChunkOffset::from(chunk_index)
                        )?
                        .is_none(),
                        "no body is cached"
                    );
                }
                assert_eq!(
                    ingress_leaf_count(tx, data_commitment, address)?,
                    0,
                    "no ingress leaf is written"
                );
                assert!(
                    irys_database::ingress_proof_by_commitment_address(
                        tx,
                        data_commitment,
                        address
                    )?
                    .is_none(),
                    "no ingress proof is made"
                );
                Ok(())
            })
            .expect("read the chunk cache");
    }

    /// The cache row is gone (pruned, or never written on this node), but a
    /// storage module holds a placement for the commitment. A chunk that this
    /// node fetched (data sync) is verified against the placement's size and
    /// lands in that module. It does not park. The cache, the ingress leaves
    /// and the ingress proofs stay empty. The chunk is not gossiped and not
    /// remembered as recently processed.
    #[tokio::test]
    async fn fetched_chunk_with_a_placement_and_no_cache_row_lands_in_the_module_only()
    -> eyre::Result<()> {
        use irys_types::{PartitionChunkOffset, partition_chunk_offset_ii};
        use nodit::interval::ii;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-no-row")
            .build();
        let (mut inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Fetched)
            .await?;

        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Data),
            "the chunk is written at its placement"
        );
        storage_module.force_sync_pending_chunks()?;
        let stored = storage_module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
        let (packed_bytes, chunk_type) = stored
            .get(&PartitionChunkOffset::from(1))
            .expect("the chunk is stored at its placement");
        assert_eq!(*chunk_type, irys_domain::ChunkType::Data);
        // The module was packed with zero entropy, so the packed bytes are the
        // chunk bytes.
        assert_eq!(packed_bytes.as_slice(), chunk.bytes.0.as_slice());
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(irys_domain::ChunkType::Entropy),
            "the other chunk of the placement is not written"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&data_commitment)
                .is_none(),
            "a fetched chunk does not park"
        );
        assert_no_cache_state(&inner, data_commitment);
        assert!(
            receivers.gossip_broadcast.try_recv().is_err(),
            "a chunk stored from its placement is not gossiped"
        );
        assert!(
            !inner
                .recent_valid_chunks
                .read()
                .await
                .contains(&chunk.chunk_key(&irys_types::data_tree::Geometry::TESTING)),
            "a chunk stored from its placement is not remembered"
        );
        Ok(())
    }

    /// A chunk first lands in its module from the placement, and its key goes
    /// in `recent_placed_chunks`. Later a header for the same commitment is
    /// cached (the data is uploaded again), and the same chunk is sent again.
    /// `recent_placed_chunks` does not stop it: it takes the cached path, its
    /// body is cached and its ingress leaf is written.
    #[tokio::test]
    async fn placed_chunk_is_cached_when_its_header_arrives_later() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-then-header")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let (_storage_module, data, data_root, proofs) =
            place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert_no_cache_state(&inner, data_commitment);

        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;

        let address = inner.config.irys_signer().address();
        inner.irys_db.view_eyre(|tx| {
            let (_, cached) =
                cached_chunk_by_chunk_offset(tx, data_commitment, TxChunkOffset::from(1))?
                    .expect("the chunk is cached once its header is known");
            assert_eq!(cached.chunk, Some(chunk.bytes.clone()));
            assert_eq!(
                ingress_leaf_count(tx, data_commitment, address)?,
                1,
                "the chunk's ingress leaf is written"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// The same data is uploaded again by a second tx with the same
    /// commitment. The first tx's cache row is pruned, but its placement
    /// remains. A chunk of the second tx arrives before its header: it is
    /// stored from the placement and also parks. When the header arrives,
    /// the drain caches the body and writes the ingress leaf. The client does
    /// not send the chunk again.
    #[tokio::test]
    async fn sent_placed_chunk_parks_and_its_header_drains_it() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};
        use irys_types::PartitionChunkOffset;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-park-drain")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;

        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Data),
            "the chunk is written at its placement"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&data_commitment)
                .is_some_and(|parked| parked.peek(&TxChunkOffset::from(1)).is_some()),
            "a sent chunk also parks under its commitment"
        );
        assert_no_cache_state(&inner, data_commitment);

        // The second tx's header arrives.
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner.process_pending_chunks_for_root(data_commitment).await;

        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&data_commitment)
                .is_none(),
            "the drain leaves nothing parked"
        );
        let address = inner.config.irys_signer().address();
        inner.irys_db.view_eyre(|tx| {
            let (_, cached) =
                cached_chunk_by_chunk_offset(tx, data_commitment, TxChunkOffset::from(1))?
                    .expect("the drain caches the body");
            assert_eq!(cached.chunk, Some(chunk.bytes.clone()));
            assert_eq!(
                ingress_leaf_count(tx, data_commitment, address)?,
                1,
                "the drain writes the ingress leaf"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// The same data is uploaded twice, each time chunks before the header.
    /// The first sent chunk parks and its header drains it, then the cache
    /// row is pruned. The same chunk is sent again: both replay caches hold
    /// its key, but nothing is parked and no ingress leaf is left, so it parks
    /// again and a new header drains it into the cache with a new leaf.
    #[tokio::test]
    async fn placed_chunk_parks_again_after_its_header_drained_it() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-repark")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let (_storage_module, data, data_root, proofs) =
            place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };
        let is_parked = |inner: &ChunkIngressServiceInner| {
            let pending = inner.pending_chunks.clone();
            async move {
                pending
                    .read()
                    .await
                    .get(&data_commitment)
                    .is_some_and(|parked| parked.peek(&TxChunkOffset::from(1)).is_some())
            }
        };

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert!(is_parked(&inner).await, "the first send parks");

        // First header: the drain caches the chunk. Then the row is pruned.
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner.process_pending_chunks_for_root(data_commitment).await;
        assert!(!is_parked(&inner).await, "the drain leaves nothing parked");
        prune_cache_row(&inner, data_commitment)?;
        assert_no_cache_state(&inner, data_commitment);

        // The same data again, before its header: the chunk parks again.
        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert!(is_parked(&inner).await, "the repeat send parks again");

        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner.process_pending_chunks_for_root(data_commitment).await;
        assert!(!is_parked(&inner).await, "the new header drains it");
        let address = inner.config.irys_signer().address();
        inner.irys_db.view_eyre(|tx| {
            let (_, cached) =
                cached_chunk_by_chunk_offset(tx, data_commitment, TxChunkOffset::from(1))?
                    .expect("the drain caches the body");
            assert_eq!(cached.chunk, Some(chunk.bytes.clone()));
            assert_eq!(
                ingress_leaf_count(tx, data_commitment, address)?,
                1,
                "the drain writes the ingress leaf again"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// The cache row is present and the same chunk is sent twice. The second
    /// send is a duplicate: it returns early and is not gossiped again.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn repeated_chunk_under_its_cache_row_is_a_duplicate() -> eyre::Result<()> {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-row-duplicate")
            .build();
        let (mut inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let data: Vec<u8> = (0..64_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };
        insert_cached_data_root(&inner.irys_db, data_root, 64);

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert!(
            receivers.gossip_broadcast.try_recv().is_ok(),
            "the first send is gossiped"
        );
        assert!(!logs_contain("already processed recently"));

        inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await?;
        assert!(
            receivers.gossip_broadcast.try_recv().is_err(),
            "the duplicate is not gossiped again"
        );
        assert!(
            logs_contain("already processed recently"),
            "the second send takes the duplicate exit"
        );
        Ok(())
    }

    /// The same data is uploaded twice, each time the header before the
    /// chunks. The first upload's cache row is pruned: that deletes its body
    /// and ingress leaf, but leaves the chunk key in `recent_valid_chunks`.
    /// The second upload's chunk is not a duplicate: its body is cached and
    /// its ingress leaf is written.
    #[tokio::test]
    async fn header_first_reupload_after_a_prune_caches_the_chunk_again() -> eyre::Result<()> {
        use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-header-first-reupload")
            .build();
        let (mut inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        inner.chunk_data_writer = super::super::chunk_data_writer::ChunkDataWriter::spawn(
            inner.irys_db.clone(),
            inner.config.irys_signer().address(),
            inner.config.data_tree_geometry(),
            8,
            &tokio::runtime::Handle::current(),
        );
        let data: Vec<u8> = (0..64_u8).collect();
        let (data_root, proofs) = data_tree_with_proofs(&data)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };
        let address = inner.config.irys_signer().address();

        // Upload 1: header, then the chunk.
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert!(receivers.gossip_broadcast.try_recv().is_ok());
        inner.irys_db.view_eyre(|tx| {
            assert_eq!(ingress_leaf_count(tx, data_commitment, address)?, 1);
            Ok(())
        })?;

        prune_cache_row(&inner, data_commitment)?;
        assert_no_cache_state(&inner, data_commitment);
        assert!(
            inner
                .recent_valid_chunks
                .read()
                .await
                .contains(&chunk.chunk_key(&irys_types::data_tree::Geometry::TESTING)),
            "the prune leaves the chunk key"
        );

        // Upload 2: header, then the same chunk.
        insert_cached_data_root(&inner.irys_db, data_root, 64);
        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        assert!(
            receivers.gossip_broadcast.try_recv().is_ok(),
            "the chunk of upload 2 is gossiped"
        );
        inner.irys_db.view_eyre(|tx| {
            let (_, cached) =
                cached_chunk_by_chunk_offset(tx, data_commitment, TxChunkOffset::from(1))?
                    .expect("the chunk of upload 2 is cached");
            assert_eq!(cached.chunk, Some(chunk.bytes.clone()));
            assert_eq!(
                ingress_leaf_count(tx, data_commitment, address)?,
                1,
                "the chunk of upload 2 writes its ingress leaf"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// A replay of a sent chunk that was stored from its placement and parked
    /// is a no-op: it does not read the placements again and does not park
    /// again.
    #[tokio::test]
    async fn replayed_placed_chunk_skips_the_placement_read() -> eyre::Result<()> {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-replay")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await?;
        // Swap the parked chunk for a marker at the same offset. A replay
        // that parks again would overwrite the marker.
        let marker = UnpackedChunk {
            bytes: vec![0xEE].into(),
            ..chunk.clone()
        };
        {
            let mut pending = inner.pending_chunks.write().await;
            assert!(
                pending
                    .get(&data_commitment)
                    .is_some_and(|parked| parked.peek(&chunk.tx_offset).is_some()),
                "the first send parks the chunk"
            );
            pending.park(marker);
        }

        // With a failing placement read, a chunk that reaches the read parks.
        PLACEMENT_READ_FAULT_MODULES.with(|faulty| *faulty.borrow_mut() = vec![storage_module.id]);
        let result = inner
            .handle_chunk_ingress_message(chunk.clone(), ChunkIngressSource::Sent)
            .await;
        PLACEMENT_READ_FAULT_MODULES.with(|faulty| faulty.borrow_mut().clear());
        result.expect("a replay is not a fault");

        assert_eq!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&data_commitment)
                .and_then(|parked| parked
                    .peek(&chunk.tx_offset)
                    .map(|held| held.bytes.0.clone())),
            Some(vec![0xEE]),
            "the replay does not reach the placement read and does not park"
        );
        Ok(())
    }

    /// One module's placement read fails. The lookup logs it and asks the
    /// next module. The error comes back only when no module answers.
    #[tokio::test]
    async fn placement_read_skips_a_failing_module() -> eyre::Result<()> {
        use irys_domain::{StorageModuleInfo, StorageModulesReadGuard};
        use irys_types::{
            PartitionChunkOffset, partition::PartitionAssignment, partition_chunk_offset_ii,
        };
        use nodit::interval::ii;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-module-fails")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let (placed_module, _data, data_root, _proofs) =
            place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let failing_module = Arc::new(StorageModule::new(
            &StorageModuleInfo {
                id: 1,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(irys_types::DataLedger::Submit.into()),
                    slot_index: Some(1),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd1".into())],
            },
            &inner.config,
        )?);
        // The failing module comes first.
        inner.storage_modules_guard =
            StorageModulesReadGuard::new(Arc::new(std::sync::RwLock::new(vec![
                Arc::clone(&failing_module),
                Arc::clone(&placed_module),
            ])));

        PLACEMENT_READ_FAULT_MODULES.with(|faulty| *faulty.borrow_mut() = vec![failing_module.id]);
        let one_failing = inner.placement_data_size_by_commitment(data_commitment);
        PLACEMENT_READ_FAULT_MODULES.with(|faulty| {
            *faulty.borrow_mut() = vec![failing_module.id, placed_module.id];
        });
        let all_failing = inner.placement_data_size_by_commitment(data_commitment);
        PLACEMENT_READ_FAULT_MODULES.with(|faulty| faulty.borrow_mut().clear());

        assert_eq!(
            one_failing?,
            Some(64),
            "the next module answers with its placement"
        );
        assert!(
            all_failing.is_err(),
            "with no module answering, the error comes back"
        );
        Ok(())
    }

    /// A failed placement read does not reject the chunk: it parks, as it did
    /// before the lookup, and it is not a fault of the sender.
    #[tokio::test]
    async fn chunk_parks_when_the_placement_read_fails() -> eyre::Result<()> {
        use irys_types::PartitionChunkOffset;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-read-fails")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        PLACEMENT_READ_FAULT_MODULES.with(|faulty| *faulty.borrow_mut() = vec![storage_module.id]);
        let result = inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await;
        PLACEMENT_READ_FAULT_MODULES.with(|faulty| faulty.borrow_mut().clear());
        result.expect("a failed placement read parks the chunk; it is not a fault");

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_commitment)
            .expect("the chunk parks under its commitment");
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        drop(pending);
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Entropy),
            "a parked chunk is not written to the module"
        );
        Ok(())
    }

    /// The module holds the placement, but its data writes are paused, so
    /// nothing stores the chunk. Ingress answers "overloaded": the sender
    /// sends it again later, and it is not a fault of the sender.
    #[tokio::test]
    async fn placed_chunk_with_paused_writes_is_overloaded() -> eyre::Result<()> {
        use irys_types::PartitionChunkOffset;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-paused")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        storage_module.pause_data_writes();
        let error = inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await
            .expect_err("a chunk that nothing stores is not accepted");
        assert!(
            matches!(
                error,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded)
            ),
            "{error:?}"
        );
        assert!(
            !crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
            "paused writes are not the sender's fault"
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Entropy)
        );
        Ok(())
    }

    /// A placement anchors the commitment. A chunk that fails against it is
    /// the sender's fault, and nothing is written.
    #[tokio::test]
    async fn wrong_chunk_under_a_placement_is_a_peer_fault() -> eyre::Result<()> {
        use irys_types::PartitionChunkOffset;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-placement-wrong-chunk")
            .build();
        let (mut inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let data_commitment = size_key(data_root, 64);
        let mut wrong_bytes = data[32..64].to_vec();
        wrong_bytes[0] ^= 1;
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 64,
            data_path: proofs[1].clone().into(),
            bytes: wrong_bytes.into(),
            tx_offset: TxChunkOffset::from(1),
        };

        let error = inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await
            .expect_err("a chunk that fails against the placement is rejected");
        assert!(
            matches!(
                error,
                ChunkIngressError::Critical(CriticalChunkIngressError::InvalidProof)
            ),
            "{error:?}"
        );
        assert!(
            crate::data_sync_service::ingress_error_is_invalid_peer_data(&error),
            "a wrong chunk under an anchored commitment is the sender's fault"
        );
        for offset in 0..2_u32 {
            assert_eq!(
                storage_module.get_chunk_type(&PartitionChunkOffset::from(offset)),
                Some(irys_domain::ChunkType::Entropy),
                "nothing is written at offset {offset}"
            );
        }
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&data_commitment)
                .is_none(),
            "a rejected chunk does not park"
        );
        assert_no_cache_state(&inner, data_commitment);
        assert!(
            receivers.gossip_broadcast.try_recv().is_err(),
            "a rejected chunk is not gossiped"
        );
        Ok(())
    }

    /// A placement for one size does not cover a chunk that claims another
    /// size. With no placement and no cache row for its own commitment, the
    /// chunk parks.
    #[tokio::test]
    async fn chunk_with_no_placement_and_no_cache_row_parks() -> eyre::Result<()> {
        use irys_types::PartitionChunkOffset;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-no-placement-no-row")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let (storage_module, data, data_root, proofs) = place_upload_without_cache_row(&mut inner)?;
        let other_size_commitment = size_key(data_root, 96);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: 96,
            data_path: proofs[1].clone().into(),
            bytes: data[32..64].to_vec().into(),
            tx_offset: TxChunkOffset::from(1),
        };

        inner
            .handle_chunk_ingress_message(chunk, ChunkIngressSource::Sent)
            .await
            .expect("a chunk that waits for its header parks; it is not a fault");

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&other_size_commitment)
            .expect("the chunk parks under its own commitment");
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        drop(pending);
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Entropy),
            "a parked chunk is not written to the module"
        );
        Ok(())
    }

    /// Closed cache writer: ingress reaches `queue_write` and fails before the
    /// batch commits. The drain must put that body back and schedule another drain.
    #[tokio::test]
    async fn drain_restores_chunk_when_precommit_database_error() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-restore")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let chunk = valid_single_chunk(vec![7_u8; 32]);
        let data_root = chunk.data_tree_root;
        let data_key = chunk.data_commitment(&irys_types::data_tree::Geometry::TESTING);
        let tx_offset = chunk.tx_offset;
        insert_cached_data_root(&inner.irys_db, data_root.0, chunk.data_size);
        inner.pending_chunks.write().await.park(chunk);

        inner.process_pending_chunks_for_root(data_key).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_key)
            .and_then(|chunks| chunks.peek(&tx_offset));
        assert!(
            parked.is_some(),
            "pre-commit database error must put the chunk back on its data root"
        );
        drop(pending);

        assert!(
            receivers.chunk_ingress.try_recv().is_err(),
            "the redrain waits instead of spinning"
        );
        let (msg, _) = tokio::time::timeout(
            PENDING_DRAIN_RETRY_DELAY * 5,
            receivers.chunk_ingress.recv(),
        )
        .await
        .expect("pre-commit database error must schedule another drain")
        .expect("ingress channel open")
        .into_parts();
        assert!(matches!(
            msg,
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_key
        ));
    }

    /// A pre-commit restore must put the popped chunk back even when parks
    /// that landed during the ingest already fill the per-root cap. Those
    /// siblings stay. The cap limits a new park, not a chunk the drain
    /// already accepted. The restore schedules another drain.
    #[tokio::test]
    async fn drain_restore_keeps_popped_chunk_when_per_root_cap_is_full() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-restore-cap")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let drained = valid_single_chunk(vec![7_u8; 32]);
        let data_root = drained.data_tree_root;
        let data_key = drained.data_commitment(&irys_types::data_tree::Geometry::TESTING);
        insert_cached_data_root(&inner.irys_db, data_root.0, drained.data_size);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 1..=cap as u32 {
                pending.park(UnpackedChunk {
                    data_tree_root: data_root,
                    data_size: drained.data_size,
                    data_path: Default::default(),
                    bytes: vec![offset as u8].into(),
                    tx_offset: TxChunkOffset::from(offset),
                });
            }
        }

        inner.finish_pending_drain(data_key, vec![drained]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_key)
            .expect("acknowledged parks stay on the root");
        assert_eq!(parked.len(), cap + 1);
        assert!(
            parked.peek(&TxChunkOffset::from(1)).is_some(),
            "restore evicted an acknowledged parked sibling"
        );
        assert!(parked.peek(&TxChunkOffset::from(2)).is_some());
        assert!(
            parked.peek(&TxChunkOffset::from(0)).is_some(),
            "a popped chunk must be restored when the per-root cap is full"
        );
        drop(pending);

        let (msg, _) = tokio::time::timeout(
            PENDING_DRAIN_RETRY_DELAY * 5,
            receivers.chunk_ingress.recv(),
        )
        .await
        .expect("a restored chunk must schedule another drain")
        .expect("ingress channel open")
        .into_parts();
        assert!(matches!(
            msg,
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_key
        ));
    }

    /// An offset parked again during the ingest is replaced. The map does not
    /// grow, and the sibling that filled the cap stays.
    #[tokio::test]
    async fn drain_restore_replaces_reparked_offset_without_growing_the_map() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-restore-repark")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let drained = valid_single_chunk(vec![7_u8; 32]);
        let data_root = drained.data_tree_root;
        let data_key = drained.data_commitment(&irys_types::data_tree::Geometry::TESTING);
        insert_cached_data_root(&inner.irys_db, data_root.0, drained.data_size);
        {
            let mut pending = inner.pending_chunks.write().await;
            pending.park(UnpackedChunk {
                data_tree_root: data_root,
                data_size: drained.data_size,
                data_path: Default::default(),
                bytes: vec![1_u8].into(),
                tx_offset: TxChunkOffset::from(0),
            });
            pending.park(UnpackedChunk {
                data_tree_root: data_root,
                data_size: drained.data_size,
                data_path: Default::default(),
                bytes: vec![2_u8].into(),
                tx_offset: TxChunkOffset::from(1),
            });
        }

        inner.finish_pending_drain(data_key, vec![drained]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_key).expect("root stays parked");
        assert_eq!(
            parked.len(),
            cap,
            "replacing a re-parked offset must not grow the map"
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("offset 0 replaced")
                .bytes
                .0,
            vec![7_u8; 32],
            "the popped chunk replaces the re-parked copy of the same offset"
        );
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        drop(pending);

        let (msg, _) = tokio::time::timeout(
            PENDING_DRAIN_RETRY_DELAY * 5,
            receivers.chunk_ingress.recv(),
        )
        .await
        .expect("replacing a popped chunk must schedule another drain")
        .expect("ingress channel open")
        .into_parts();
        assert!(matches!(
            msg,
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_key
        ));
    }

    /// A pre-header chunk that arrives during a drain is still rejected when the
    /// popped set already fills the cap. The popped chunks are restored. After
    /// the flag clears, the cap reject still applies.
    #[tokio::test]
    async fn pending_drain_rejects_a_new_preheader_chunk_when_the_popped_set_fills_the_cap() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-preheader")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x55; 32]);
        park_offsets(&inner, data_root, 0..cap as u32).await;
        let (popped, _ticket) = pop_for_drain(&inner, data_root).await;
        let incoming = chunk_with(data_root, cap as u32, 0xAB, CLAIMED_DATA_SIZE);

        let err = inner
            .handle_chunk_ingress_message(incoming.clone(), ChunkIngressSource::Sent)
            .await
            .expect_err("a pre-header chunk must be rejected while the root is draining");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap)
            ),
            "{err:?}"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&claim_key(data_root))
                .is_none(),
            "a rejected chunk must not occupy the drained root"
        );

        inner
            .finish_pending_drain(claim_key(data_root), popped)
            .await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("popped chunks restored");
            assert_eq!(parked.len(), cap);
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(0))
                    .expect("popped offset 0")
                    .bytes
                    .0,
                vec![0]
            );
            assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
            assert!(
                parked.peek(&TxChunkOffset::from(cap as u32)).is_none(),
                "a chunk past the cap is not parked"
            );
        }
        expect_no_redrain(&mut receivers).await;

        let err = inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect_err("after the drain, a chunk past the cap still rejects");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap)
            ),
            "{err:?}"
        );
        let pending = inner.pending_chunks.read().await;
        assert_eq!(
            pending
                .get(&claim_key(data_root))
                .expect("root parked")
                .len(),
            cap
        );
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// The same offset arriving during a drain is replaced by the popped bytes.
    /// The offset stays. One follow-up runs, then stops.
    #[tokio::test]
    async fn same_offset_arriving_during_drain_keeps_the_popped_bytes() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-same-offset")
            .build();
        let cap = 4_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x5A; 32]);
        park_offsets(&inner, data_root, [0_u32]).await;
        let (popped, ticket) = pop_for_drain(&inner, data_root).await;
        inner
            .handle_chunk_ingress_message(
                chunk_with(data_root, 0, 0xAB, CLAIMED_DATA_SIZE),
                ChunkIngressSource::Sent,
            )
            .await
            .expect("the same offset parks while the root is draining");

        inner
            .finish_pending_drain(claim_key(data_root), popped)
            .await;
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("offset stays parked");
            assert_eq!(parked.len(), 1);
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(0))
                    .expect("popped bytes replace the arrival")
                    .bytes
                    .0,
                vec![0]
            );
        }
        expect_delayed_redrain(&mut receivers, data_root).await;
        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;
        expect_no_redrain(&mut receivers).await;
    }

    /// Cancel drops the popped vec with the task. A chunk that parked during
    /// the drain stays in the map, and the ticket schedules one follow-up.
    #[tokio::test]
    async fn chunk_parked_during_drain_survives_cancel() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-cancel")
            .build();
        let cap = 4_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0xC1; 32]);
        park_offsets(&inner, data_root, [0_u32]).await;
        let (popped, ticket) = pop_for_drain(&inner, data_root).await;
        inner
            .handle_chunk_ingress_message(
                chunk_with(data_root, 1, 1, CLAIMED_DATA_SIZE),
                ChunkIngressSource::Sent,
            )
            .await
            .expect("the arrival parks before cancel");
        drop(popped);
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            assert!(
                !pending.is_draining(&claim_key(data_root)),
                "cancel clears the drain flag"
            );
            let parked = pending
                .get(&claim_key(data_root))
                .expect("the arrival is still parked");
            assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
            assert!(
                parked.peek(&TxChunkOffset::from(0)).is_none(),
                "the popped chunk was still in the cancelled task"
            );
        }
        expect_delayed_redrain(&mut receivers, data_root).await;
        expect_no_redrain(&mut receivers).await;
    }

    /// A chunk that arrives while its root is draining is not a cap reject.
    /// It stays parked. The drain schedules one follow-up. The follow-up does
    /// not schedule again when no further chunk arrives.
    #[tokio::test]
    async fn chunk_that_arrives_during_drain_stays_parked_and_one_follow_up_runs() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-arrival")
            .build();
        let cap = 4_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0xA7; 32]);
        park_offsets(&inner, data_root, [0_u32]).await;
        let (popped, ticket) = pop_for_drain(&inner, data_root).await;

        for offset in [1_u32, 2] {
            inner
                .handle_chunk_ingress_message(
                    chunk_with(data_root, offset, offset as u8, CLAIMED_DATA_SIZE),
                    ChunkIngressSource::Sent,
                )
                .await
                .expect("a chunk under the cap must be kept while the root is draining");
        }

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("chunks that arrived during the drain stay parked");
            assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
            assert!(parked.peek(&TxChunkOffset::from(2)).is_some());
            assert!(
                parked.peek(&TxChunkOffset::from(0)).is_none(),
                "the popped chunk is restored when the drain finishes"
            );
        }
        assert!(
            receivers.chunk_ingress.try_recv().is_err(),
            "the in-flight drain schedules the follow-up, not each park"
        );

        inner
            .finish_pending_drain(claim_key(data_root), popped)
            .await;
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("popped chunk restored");
            assert_eq!(parked.len(), 3);
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(0))
                    .expect("popped offset")
                    .bytes
                    .0,
                vec![0]
            );
            assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
            assert!(parked.peek(&TxChunkOffset::from(2)).is_some());
        }
        expect_delayed_redrain(&mut receivers, data_root).await;

        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("follow-up with no header leaves the chunks parked");
            assert_eq!(parked.len(), 3);
        }
        expect_no_redrain(&mut receivers).await;

        inner
            .handle_chunk_ingress_message(
                chunk_with(data_root, 3, 3, 128),
                ChunkIngressSource::Sent,
            )
            .await
            .expect("after the drain, a chunk of another size parks");
        {
            let pending = inner.pending_chunks.read().await;
            assert_eq!(
                pending
                    .get(&claim_key(data_root))
                    .expect("root parked")
                    .len(),
                3,
                "another size does not join this commitment"
            );
            assert_eq!(
                pending
                    .get(&size_key(data_root, 128))
                    .expect("the other size parks under its own commitment")
                    .len(),
                1
            );
        }
        expect_no_redrain(&mut receivers).await;
    }

    /// The same offset was parked again while the drain held it, and the map
    /// is full. Put the popped bytes back over that offset. Do not grow.
    #[tokio::test]
    async fn ingress_cap_repark_replaces_same_offset_without_growing() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-repark-replace")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x66; 32]);
        insert_cached_data_root(&inner.irys_db, data_root, 32);
        park_offsets(&inner, data_root, 0..=1).await;
        let popped = chunk_with(data_root, 0, 0xAB, CLAIMED_DATA_SIZE);

        inner
            .finish_pending_drain(claim_key(data_root), vec![popped])
            .await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("root stays parked");
        assert_eq!(
            parked.len(),
            cap,
            "a re-parked offset must not grow the map"
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("offset 0 replaced")
                .bytes
                .0,
            vec![0xAB],
            "the popped chunk replaces the re-parked copy of the same offset"
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("sibling stays")
                .bytes
                .0,
            vec![1]
        );
        drop(pending);
        // Replacing an offset that is already parked makes no progress.
        // Another drain would pop the same full root and replace it again.
        expect_no_redrain(&mut receivers).await;
    }

    /// Cap restores of new offsets stop at the configured per-root cap.
    /// Accepted siblings that already occupy the cap stay. Further offsets
    /// are not inserted, and a cap-only restore does not schedule a drain.
    #[tokio::test]
    async fn cap_restore_stops_growing_once_the_per_root_ceiling_is_full() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-ceiling")
            .build();
        let cap = 2_usize;
        // chunk_size in the test config is 32. 224 bytes covers offsets 0..=6.
        let data_size = 224_u64;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x77; 32]);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 0..cap as u32 {
                pending.park(chunk_with(data_root, offset, offset as u8, data_size));
            }
        }

        for offset in cap as u32..=6 {
            inner
                .finish_pending_drain(
                    size_key(data_root, data_size),
                    vec![chunk_with(data_root, offset, offset as u8, data_size)],
                )
                .await;
        }

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&size_key(data_root, data_size))
            .expect("root stays parked");
        assert_eq!(
            parked.len(),
            cap,
            "cap restores must not grow the per-root map"
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("accepted offset 0 stays")
                .bytes
                .0,
            vec![0]
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("accepted offset 1 stays")
                .bytes
                .0,
            vec![1]
        );
        for offset in cap as u32..=6 {
            assert!(
                parked.peek(&TxChunkOffset::from(offset)).is_none(),
                "offset {offset} is past the per-root ceiling"
            );
        }
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// A root that is already above the cap keeps every popped chunk on a
    /// drain, and does not schedule another drain when every chunk is a cap
    /// reject. A second drain stays at the same length.
    #[tokio::test]
    async fn over_cap_drain_keeps_popped_chunks_and_does_not_redrain() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-over-cap")
            .build();
        let cap = 2_usize;
        let extra = 3_usize;
        let total = cap + extra;
        let data_size = 224_u64;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x79; 32]);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 0..cap as u32 {
                pending.park(chunk_with(data_root, offset, offset as u8, data_size));
            }
            let cache = pending
                .get_mut(&size_key(data_root, data_size))
                .expect("root parked");
            cache.resize(std::num::NonZeroUsize::new(total).expect("over-cap size"));
            for offset in cap as u32..total as u32 {
                cache.put(
                    TxChunkOffset::from(offset),
                    chunk_with(data_root, offset, offset as u8, data_size),
                );
            }
        }

        inner
            .process_pending_chunks_for_root(size_key(data_root, data_size))
            .await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&size_key(data_root, data_size))
                .expect("popped set is restored");
            assert_eq!(parked.len(), total, "an accepted popped chunk must stay");
            for offset in 0..total as u32 {
                assert_eq!(
                    parked
                        .peek(&TxChunkOffset::from(offset))
                        .unwrap_or_else(|| panic!("offset {offset} dropped"))
                        .bytes
                        .0,
                    vec![offset as u8]
                );
            }
        }
        expect_no_redrain(&mut receivers).await;

        inner
            .process_pending_chunks_for_root(size_key(data_root, data_size))
            .await;
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&size_key(data_root, data_size))
            .expect("second drain restores the same set");
        assert_eq!(parked.len(), total, "a second cap-only drain must not grow");
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// A permanent reject in the same drain stays dropped. Cap rejects from
    /// the popped set are put back and do not schedule another drain.
    #[tokio::test]
    async fn ingress_cap_repark_keeps_only_the_cap_rejected_chunk() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-repark-mixed")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x88; 32]);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 0..cap as u32 {
                pending.park(chunk_with(
                    data_root,
                    offset,
                    offset as u8,
                    CLAIMED_DATA_SIZE,
                ));
            }
            let cache = pending.get_mut(&claim_key(data_root)).expect("root parked");
            cache.resize(std::num::NonZeroUsize::new(cap + 1).expect("room for the reject"));
            let mut oversized = chunk_with(data_root, 3, 0x03, CLAIMED_DATA_SIZE);
            oversized.bytes = vec![1_u8; 33].into();
            cache.put(TxChunkOffset::from(3), oversized);
        }

        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("cap rejects are restored");
        assert!(
            parked.peek(&TxChunkOffset::from(3)).is_none(),
            "oversized pre-header bytes stay dropped"
        );
        assert_eq!(parked.len(), cap);
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("offset 0 restored")
                .bytes
                .0,
            vec![0]
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("offset 1 restored")
                .bytes
                .0,
            vec![1]
        );
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// One chunk was already processed under the current cache row (a recent
    /// key, the row and this node's ingress leaf), so its re-ingest returns
    /// Ok. The sibling is a cap restore. That schedules one drain. The next
    /// pass is cap-only and does not schedule again.
    ///
    /// A cache row covers every chunk of its commitment. So in one drain, an
    /// Ok and a cap restore need the row to change between two chunks (a
    /// header or a prune during the drain). To get the cap restore without
    /// that race, the sibling claims another size: it has no row, and the
    /// pre-header entry of its own commitment is full.
    #[tokio::test]
    async fn ok_chunk_and_cap_restore_schedules_one_drain_then_stops() -> eyre::Result<()> {
        use irys_database::{db::IrysDatabaseExt as _, store_ingress_data_hash};

        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-one-drain")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x46; 32]);
        let other_data_size = 128;
        let seen = chunk_with(data_root, 0, 0, CLAIMED_DATA_SIZE);
        let other = chunk_with(data_root, 1, 1, other_data_size);
        {
            let mut pending = inner.pending_chunks.write().await;
            pending.park(seen.clone());
            pending.put(claim_key(data_root), other);
            pending.park(chunk_with(data_root, 0, 0xA0, other_data_size));
            pending.park(chunk_with(data_root, 2, 0xA2, other_data_size));
        }
        insert_cached_data_root(&inner.irys_db, data_root, CLAIMED_DATA_SIZE);
        let address = inner.config.irys_signer().address();
        inner.irys_db.update_eyre(|tx| {
            store_ingress_data_hash(
                tx,
                claim_key(data_root),
                address,
                seen.tx_offset,
                H256::zero(),
            )?;
            Ok(())
        })?;
        inner.recent_valid_chunks.write().await.put(
            seen.chunk_key(&irys_types::data_tree::Geometry::TESTING),
            (),
        );

        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&claim_key(data_root))
                .expect("cap sibling restored");
            assert!(
                parked.peek(&TxChunkOffset::from(0)).is_none(),
                "the chunk that returned Ok is not put back"
            );
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(1))
                    .expect("cap sibling stays")
                    .bytes
                    .0,
                vec![1]
            );
            assert_eq!(parked.len(), 1);
        }
        expect_delayed_redrain(&mut receivers, data_root).await;

        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("second pass restores the sibling");
        assert_eq!(parked.len(), 1);
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        drop(pending);
        expect_no_redrain(&mut receivers).await;
        Ok(())
    }

    /// Popping a full root restores every chunk. Parks during that re-ingest
    /// are rejected, so the restore puts the popped set back and does not
    /// schedule another drain.
    #[tokio::test]
    async fn ingress_cap_full_root_drain_parks_every_popped_chunk() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-full-root")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x99; 32]);
        park_offsets(&inner, data_root, 0..cap as u32).await;

        inner
            .process_pending_chunks_for_root(claim_key(data_root))
            .await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("full root parks again");
        assert_eq!(parked.len(), cap);
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("offset 0")
                .bytes
                .0,
            vec![0]
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("offset 1")
                .bytes
                .0,
            vec![1]
        );
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// A popped chunk that still fits parks on the success path. No retry.
    #[tokio::test]
    async fn ingress_cap_under_cap_parks_popped_preheader_chunk() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-under")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0xAA; 32]);
        park_offsets(&inner, data_root, [1_u32]).await;
        let popped = chunk_with(data_root, 0, 0xAB, CLAIMED_DATA_SIZE);

        inner
            .finish_pending_drain(claim_key(data_root), vec![popped])
            .await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("root stays parked");
        assert_eq!(parked.len(), 2);
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("popped chunk parks")
                .bytes
                .0,
            vec![0xAB]
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("sibling")
                .bytes
                .0,
            vec![1]
        );
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// A new ingress, not a drain, still rejects when the cap is full.
    #[tokio::test]
    async fn ingress_cap_preheader_park_rejects_a_new_chunk() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-first-preheader")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0xBB; 32]);
        park_offsets(&inner, data_root, 0..cap as u32).await;
        let incoming = chunk_with(data_root, 2, 0xAB, CLAIMED_DATA_SIZE);

        let err = inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect_err("a new pre-header chunk must still reject at the cap");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap)
            ),
            "{err:?}"
        );
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&claim_key(data_root))
            .expect("root stays parked");
        assert_eq!(parked.len(), cap);
        assert!(parked.peek(&TxChunkOffset::from(2)).is_none());
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(0))
                .expect("offset 0")
                .bytes
                .0,
            vec![0]
        );
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("offset 1")
                .bytes
                .0,
            vec![1]
        );
        drop(pending);
        expect_no_redrain(&mut receivers).await;
    }

    /// Uploads that `test_ingress_inner` parks at once.
    const PENDING_ENTRY_CAP: usize = 8;

    /// One chunk of the test config (`chunk_size` 32) is not enough for offsets 0..=2.
    const CLAIMED_DATA_SIZE: u64 = 96;

    /// Key of a chunk that claims `data_size` for `data_root`.
    fn size_key(data_root: H256, data_size: u64) -> DataCommitment {
        irys_types::data_tree::data_commitment(
            &irys_types::data_tree::Geometry::TESTING,
            DataTreeRoot(data_root),
            data_size,
        )
    }

    /// Key of a chunk that claims `CLAIMED_DATA_SIZE` for `data_root`.
    fn claim_key(data_root: H256) -> DataCommitment {
        size_key(data_root, CLAIMED_DATA_SIZE)
    }

    fn chunk_with(data_root: H256, offset: u32, byte: u8, data_size: u64) -> UnpackedChunk {
        UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size,
            data_path: Default::default(),
            bytes: vec![byte].into(),
            tx_offset: TxChunkOffset::from(offset),
        }
    }

    async fn park_offsets(
        inner: &ChunkIngressServiceInner,
        data_root: H256,
        offsets: impl IntoIterator<Item = u32>,
    ) {
        let mut pending = inner.pending_chunks.write().await;
        for offset in offsets {
            pending.park(chunk_with(
                data_root,
                offset,
                offset as u8,
                CLAIMED_DATA_SIZE,
            ));
        }
    }

    async fn pop_for_drain(
        inner: &ChunkIngressServiceInner,
        data_root: H256,
    ) -> (
        Vec<UnpackedChunk>,
        crate::chunk_ingress_service::pending_chunks::DrainTicket,
    ) {
        let mut pending = inner.pending_chunks.write().await;
        let chunks_map = pending.pop(&claim_key(data_root)).expect("parked root");
        let mut ticket = pending.begin_drain(claim_key(data_root), chunks_map.len());
        let senders = inner.service_senders.clone();
        ticket.set_gained_follow_up(move || {
            schedule_pending_drain_retry(senders, claim_key(data_root));
        });
        let chunks = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
        (chunks, ticket)
    }

    async fn expect_delayed_redrain(
        receivers: &mut crate::services::ServiceReceivers,
        data_root: H256,
    ) {
        assert!(
            receivers.chunk_ingress.try_recv().is_err(),
            "the redrain waits instead of spinning"
        );
        let (msg, _) = tokio::time::timeout(
            PENDING_DRAIN_RETRY_DELAY * 5,
            receivers.chunk_ingress.recv(),
        )
        .await
        .expect("a restored chunk must schedule another drain")
        .expect("ingress channel open")
        .into_parts();
        assert!(matches!(
            msg,
            ChunkIngressMessage::ProcessPendingChunks(root) if root == claim_key(data_root)
        ));
    }

    async fn expect_no_redrain(receivers: &mut crate::services::ServiceReceivers) {
        let received = tokio::time::timeout(
            PENDING_DRAIN_RETRY_DELAY * 2,
            receivers.chunk_ingress.recv(),
        )
        .await;
        assert!(
            received.is_err(),
            "this drain must not schedule another ProcessPendingChunks"
        );
    }

    /// A validation reject is permanent. The drain must not put it back.
    #[tokio::test]
    async fn drain_drops_validation_failure_without_redrain() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-drop")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let chunk = UnpackedChunk {
            data_tree_root: DataTreeRoot(H256([0x11; 32])),
            data_size: 33,
            data_path: Default::default(),
            bytes: vec![1_u8; 33].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        let data_key = chunk.data_commitment(&irys_types::data_tree::Geometry::TESTING);
        let tx_offset = chunk.tx_offset;
        inner.pending_chunks.write().await.park(chunk);

        inner.process_pending_chunks_for_root(data_key).await;

        let pending = inner.pending_chunks.read().await;
        assert!(
            pending
                .get(&data_key)
                .and_then(|chunks| chunks.peek(&tx_offset))
                .is_none(),
            "validation failure must stay dropped"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// Another size of the root is another commitment. A claim parks under its
    /// own key: it neither evicts nor shares the cap of a full sibling size.
    #[tokio::test]
    async fn claim_of_another_size_parks_without_sharing_a_full_cap() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-cap")
            .build();
        let (inner, _receivers) = test_ingress_inner(&tmp, 2, 2);
        let data_root = H256([0x22; 32]);
        insert_cached_data_root(&inner.irys_db, data_root, 10);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 0..2_u32 {
                pending.park(UnpackedChunk {
                    data_tree_root: DataTreeRoot(data_root),
                    data_size: 20,
                    data_path: Default::default(),
                    bytes: vec![offset as u8].into(),
                    tx_offset: TxChunkOffset::from(offset),
                });
            }
        }
        // `CLAIMED_DATA_SIZE` covers offset 2 at the test `chunk_size` of 32.
        let incoming = UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size: CLAIMED_DATA_SIZE,
            data_path: Default::default(),
            bytes: vec![9_u8].into(),
            tx_offset: TxChunkOffset::from(2),
        };
        inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect("a claim with no header parks; it is not a peer fault");
        let pending = inner.pending_chunks.read().await;
        let sibling = pending
            .get(&size_key(data_root, 20))
            .expect("the full sibling size stays parked");
        assert_eq!(sibling.len(), 2);
        let claim = pending
            .get(&claim_key(data_root))
            .expect("the claim parks under its own commitment");
        assert_eq!(claim.len(), 1);
        assert!(claim.peek(&TxChunkOffset::from(2)).is_some());
    }

    /// A chunk that waits for its header is not proof-checked, so a body
    /// over `chunk_size` is rejected and not parked.
    #[tokio::test]
    async fn preheader_chunk_rejects_body_over_chunk_size() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-oversize-bytes")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC2; 32]);
        let mut incoming = chunk_with(data_root, 0, 0xAB, 33);
        // Test config `chunk_size` is 32. One byte over must not park.
        incoming.bytes = vec![1_u8; 33].into();

        let err = inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect_err("a body over chunk_size must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedBytes)
            ),
            "{err:?}"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&size_key(data_root, 33))
                .is_none(),
            "a rejected chunk must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// A chunk that waits for its header, with a proof longer than the pre-header max.
    #[tokio::test]
    async fn preheader_chunk_rejects_data_path_over_preheader_max() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-oversize-path")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC3; 32]);
        let mut incoming = chunk_with(data_root, 0, 0xAB, 64);
        let max_path = inner.config.mempool.max_preheader_data_path_bytes;
        incoming.data_path = vec![0_u8; max_path + 1].into();

        let err = inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect_err("a data_path over the pre-header max must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedDataPath)
            ),
            "{err:?}"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&size_key(data_root, 64))
                .is_none(),
            "a rejected chunk must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// `chunk_size` 32 and `data_size` 32 admit only offset 0.
    #[tokio::test]
    async fn preheader_chunk_rejects_offset_past_claimed_size() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-bad-offset")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC4; 32]);
        let incoming = chunk_with(data_root, 1, 0xAB, 32);

        let err = inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect_err("a tx_offset past the claimed size must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderInvalidOffset(_))
            ),
            "{err:?}"
        );
        assert!(
            inner
                .pending_chunks
                .read()
                .await
                .get(&size_key(data_root, 32))
                .is_none(),
            "a rejected chunk must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// A chunk inside the body, proof, and offset bounds is parked.
    /// Bytes equal `chunk_size` and the proof equals the configured max.
    #[tokio::test]
    async fn preheader_chunk_parks_when_body_path_and_offset_fit() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-bounded")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC5; 32]);
        // 64 bytes is two chunks of 32, so offset 1 is the last valid offset.
        let mut incoming = chunk_with(data_root, 1, 0xAB, 64);
        incoming.bytes = vec![1_u8; 32].into();
        let max_path = inner.config.mempool.max_preheader_data_path_bytes;
        incoming.data_path = vec![0_u8; max_path].into();

        inner
            .handle_chunk_ingress_message(incoming, ChunkIngressSource::Sent)
            .await
            .expect("a bounded chunk parks");
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&size_key(data_root, 64))
            .expect("chunk is parked");
        assert_eq!(parked.len(), 1);
        assert_eq!(
            parked
                .peek(&TxChunkOffset::from(1))
                .expect("offset 1")
                .bytes
                .0
                .len(),
            32
        );
        drop(pending);
        assert!(
            receivers.chunk_ingress.try_recv().is_err(),
            "no header names this size, so no drain is scheduled"
        );
    }

    fn test_ingress_inner(
        tmp: &impl AsRef<std::path::Path>,
        max_chunks_per_item: usize,
        preheader_cap: usize,
    ) -> (ChunkIngressServiceInner, crate::services::ServiceReceivers) {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{BlockTree, BlockTreeReadGuard, StorageModulesReadGuard};
        use irys_testing_utils::new_mock_signed_header;
        use irys_types::{Config, NodeConfig};
        use reth::tasks::TaskExecutor;
        use reth_db::mdbx::DatabaseArguments;
        use std::num::NonZeroUsize;

        let mut node_config = NodeConfig::testing();
        {
            let consensus = node_config.consensus.get_mut();
            consensus.chunk_size = 32;
            consensus.num_chunks_in_partition = 5;
        }
        node_config.base_directory = tmp.as_ref().to_path_buf();
        node_config.mempool.max_chunks_per_item = max_chunks_per_item;
        node_config.mempool.max_preheader_chunks_per_item = preheader_cap;
        let config = Config::new_with_random_peer_id(node_config);

        let db_env = open_or_create_db(
            tmp.as_ref().join("db"),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing database arguments"),
        )
        .expect("open test database");
        let irys_db = DatabaseProvider(std::sync::Arc::new(db_env));

        let (service_senders, receivers) = crate::test_helpers::build_test_service_senders();
        let block_tree_read_guard =
            BlockTreeReadGuard::new(std::sync::Arc::new(std::sync::RwLock::new(BlockTree::new(
                &new_mock_signed_header(),
                config.consensus.clone(),
            ))));
        let inner = ChunkIngressServiceInner {
            block_tree_read_guard,
            config,
            exec: TaskExecutor::test(),
            irys_db,
            message_handler_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            control_plane_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            max_concurrent_tasks: 1,
            max_control_plane_tasks: 1,
            service_senders,
            storage_modules_guard: StorageModulesReadGuard::new(std::sync::Arc::new(
                std::sync::RwLock::new(Vec::new()),
            )),
            recent_valid_chunks: RwLock::new(LruCache::new(NonZeroUsize::new(8).expect("nonzero"))),
            recent_placed_chunks: RwLock::new(LruCache::new(
                NonZeroUsize::new(8).expect("nonzero"),
            )),
            pending_chunks: std::sync::Arc::new(RwLock::new(
                super::super::PriorityPendingChunks::new(PENDING_ENTRY_CAP, preheader_cap),
            )),
            pending_ingress_proofs: std::sync::Mutex::new(
                super::super::pending_ingress_proofs::PendingIngressProofs::new(8),
            ),
            chunk_data_writer: super::super::chunk_data_writer::ChunkDataWriter::closed_for_test(),
            ingress_proof_generation_state: super::super::IngressProofGenerationState::new(),
            http_ingress_open: std::sync::Arc::new(std::sync::Mutex::new(true)),
        };
        (inner, receivers)
    }

    fn insert_cached_data_root(db: &DatabaseProvider, data_root: H256, data_size: u64) {
        use irys_database::cache_data_root;
        use irys_database::db::IrysDatabaseExt as _;
        use irys_types::{
            DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata,
        };

        let header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_tree_root: DataTreeRoot(data_root),
                data_size,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_eyre(|tx| {
            cache_data_root(tx, &header, None, &irys_types::data_tree::Geometry::TESTING)
                .map(|_| ())
        })
        .expect("insert cached data root");
    }

    fn valid_single_chunk(bytes: Vec<u8>) -> UnpackedChunk {
        let data_size = bytes.len() as u64;
        let (data_root, proofs) = data_tree_with_proofs(&bytes).expect("one-chunk data tree");
        UnpackedChunk {
            data_tree_root: DataTreeRoot(data_root),
            data_size,
            data_path: proofs[0].clone().into(),
            bytes: bytes.into(),
            tx_offset: TxChunkOffset::from(0),
        }
    }
}
